//! `LongTermMemory` — the curated long-term `MemoryPort` adapter (Story 11.2).
//!
//! Maintains a single, topic-organized markdown file at
//! `{workspace}/.rustain/MEMORY.md` (a SIBLING of the daily-log `memory/`
//! directory, NOT inside it — architecture.md:575). This is the OPPOSITE tier
//! from `DailyLogMemory` (11.1):
//!
//! | aspect      | DailyLogMemory (11.1)            | LongTermMemory (this story)        |
//! |-------------|----------------------------------|------------------------------------|
//! | layout      | `memory/YYYY-MM-DD.md` (time)    | `MEMORY.md` (topic — `## Category`)|
//! | write model | append-only (`OpenOptions`)      | rewrite-on-upsert (`fs::write`)    |
//! | load model  | once-only (`OnceCell`)           | mtime-checked `ensure_fresh()`     |
//! | record      | `MemoryEntry` (operational)      | `MemoryFact` (curated/durable)     |
//!
//! Design notes (see story Dev Notes):
//! - **Human-editable + reload (AC2/AC5)**: `ensure_fresh()` `stat`s the file on
//!   every `recent`/`search`/`remember_fact`; if the on-disk mtime differs from
//!   the loaded snapshot it re-parses, so manual hand-edits (add/update/remove)
//!   are picked up mid-session. A missing file is empty state, not an error.
//! - **Curated, not append-only**: `remember_fact` parses → upserts → rewrites
//!   the whole file via `tokio::fs::write` (the deliberate contrast with
//!   daily-log). Durable facts are deduped within their topic.
//! - **Size warning (AC3)**: once per session (an `AtomicBool` CAS guards it),
//!   the adapter emits a `SystemNotice` warning if `MEMORY.md` exceeds 20KB.
//! - **Lock policy (CLAUDE.md)**: `loaded` is a `tokio::sync::RwLock`; read
//!   guards are scoped tightly. Write guards in `remember_fact` span the
//!   disk write to prevent concurrent-writer data loss (tokio's RwLock safely
//!   holds across `.await`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use async_trait::async_trait;
use chrono::{DateTime, Local};
use tokio::sync::RwLock;
use tokio::sync::mpsc::UnboundedSender;

use crate::domain::errors::MemoryError;
use crate::domain::events::AppEvent;
use crate::domain::models::{MemoryEntry, MemoryFact, NoticeLevel};
use crate::domain::ports::MemoryPort;

/// File size beyond which the size/cost warning fires (AC3).
const SIZE_WARN_BYTES: u64 = 20 * 1024;

/// Normalize text for dedup: trim, collapse internal whitespace, lowercase.
/// Two facts/entries are "the same information" iff their normalized text is
/// equal. Shared by `LongTermMemory` (within-category dedup) and
/// `ProjectScopedMemory` (cross-tier dedup, long-term wins).
pub(crate) fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// In-memory snapshot of `MEMORY.md`, preserving category (section) order.
#[derive(Default)]
struct LoadedState {
    /// `(category, facts)` in file order. One section per distinct category.
    sections: Vec<(String, Vec<MemoryFact>)>,
    /// The file mtime the snapshot was parsed from (`None` = missing file /
    /// platform without mtime). Drives `ensure_fresh`'s reload decision.
    mtime: Option<SystemTime>,
    /// Whether a load has been attempted at least once (distinguishes
    /// "never loaded" from "loaded an empty/missing file").
    loaded_once: bool,
}

/// File-backed curated long-term memory adapter.
pub struct LongTermMemory {
    /// `{workspace}/.rustain/MEMORY.md` — resolved once at construction (no I/O).
    memory_file: PathBuf,
    /// In-memory snapshot, reloaded on mtime change (`ensure_fresh`).
    ///
    /// **Drain seam (Story 12.0 C3 / AC2, AC3, AC9).** `remember_fact` holds the
    /// WRITE guard across its whole read-modify-render-`fs::write` window (Story
    /// 16-0 AC2). `prepare_detach()` therefore drains an in-flight upsert simply
    /// by acquiring (and dropping) this same write lock — the swap cannot complete
    /// until the curated write is durable. This is the ONE hardened write sink the
    /// profile swap funnels through (AC9): do NOT add a second per-call-site gate.
    loaded: RwLock<LoadedState>,
    /// Event bus for the once-per-session size warning. `None` (headless/eval)
    /// silently skips the notice.
    event_tx: Option<UnboundedSender<AppEvent>>,
    /// Once-per-session guard for the size warning (AC3). CAS to `true` on emit.
    warned: AtomicBool,
    /// Test-only deterministic suspension seam (Story 12.0 AC10) — compiled out of
    /// release builds.
    #[cfg(test)]
    seam: WriteSeam,
}

/// Test-only suspension seam pinning the C3 interior TOCTOU window between the
/// DF-1 re-stat and the `tokio::fs::write` (Story 12.0 AC10). A test arms it; the
/// first in-flight `remember_fact` to reach the seam disarms it, signals
/// `reached`, and parks on `proceed` — all while holding the `loaded` write guard,
/// so a concurrent `prepare_detach()` drain blocks behind it. Lets the C2/C3
/// profile-swap lost-write / lost-update race be pinned deterministically without
/// a `sleep`.
#[cfg(test)]
#[derive(Default)]
struct WriteSeam {
    armed: AtomicBool,
    reached: tokio::sync::Notify,
    proceed: tokio::sync::Notify,
}

impl LongTermMemory {
    /// Construct an adapter rooted at `{workspace}/.rustain/MEMORY.md`.
    /// Does NO I/O — the file is read on first `recent`/`search`/`remember_fact`
    /// (or an explicit `initialize`), and the `.rustain/` dir is created on the
    /// first `remember_fact` write.
    pub fn new(workspace_path: &Path) -> Self {
        Self {
            memory_file: workspace_path.join(".rustain").join("MEMORY.md"),
            loaded: RwLock::new(LoadedState::default()),
            event_tx: None,
            warned: AtomicBool::new(false),
            #[cfg(test)]
            seam: WriteSeam::default(),
        }
    }

    /// Wire the event bus so the size-warning `SystemNotice` can surface
    /// (mirrors `ToolSetAdapter::set_event_tx`). Called at the composition root.
    pub fn set_event_tx(&mut self, event_tx: UnboundedSender<AppEvent>) {
        self.event_tx = Some(event_tx);
    }

    /// Load `MEMORY.md` into the snapshot (public for startup/tests). Idempotent
    /// — a no-op when the file is unchanged since the last load. Missing file →
    /// empty state, not an error (AC5).
    pub async fn initialize(&self) -> Result<(), MemoryError> {
        self.ensure_fresh().await
    }

    /// Reload the snapshot iff `MEMORY.md`'s mtime changed since the last load
    /// (or it was never loaded). The reload primitive behind AC2 (manual edits
    /// picked up) and AC5 (empty state on a missing file).
    async fn ensure_fresh(&self) -> Result<(), MemoryError> {
        let meta = match tokio::fs::metadata(&self.memory_file).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Missing file → empty state (AC5). Clear any stale snapshot.
                let mut guard = self.loaded.write().await;
                guard.sections.clear();
                guard.mtime = None;
                guard.loaded_once = true;
                return Ok(());
            }
            Err(e) => {
                return Err(MemoryError::IoError(format!(
                    "failed to stat {}: {}",
                    self.memory_file.display(),
                    e
                )));
            }
        };
        let disk_mtime = meta.modified().ok();

        // Skip the re-parse when we have a snapshot at the same mtime. When
        // both sides lack mtime (exotic platforms), skip after first load to
        // avoid redundant I/O on every call (DF-2 code review fix).
        {
            let guard = self.loaded.read().await;
            if guard.loaded_once {
                if guard.mtime.is_some() && guard.mtime == disk_mtime {
                    return Ok(());
                }
                if guard.mtime.is_none() && disk_mtime.is_none() {
                    return Ok(());
                }
            }
        }

        let content = match tokio::fs::read_to_string(&self.memory_file).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Raced with a delete between stat and read — treat as empty.
                let mut guard = self.loaded.write().await;
                guard.sections.clear();
                guard.mtime = None;
                guard.loaded_once = true;
                return Ok(());
            }
            Err(e) => {
                return Err(MemoryError::IoError(format!(
                    "failed to read {}: {}",
                    self.memory_file.display(),
                    e
                )));
            }
        };

        let sections = Self::parse(&content);
        {
            let mut guard = self.loaded.write().await;
            guard.sections = sections;
            guard.mtime = disk_mtime;
            guard.loaded_once = true;
        }

        // Size warning fires on the load that crosses 20KB — once per session.
        self.maybe_warn_size(meta.len());
        Ok(())
    }

    /// Emit the once-per-session size warning if `size` exceeds the 20KB
    /// threshold (AC3). The `AtomicBool` CAS guarantees a single emission; a
    /// `None` `event_tx` (headless/eval) skips silently without consuming the
    /// flag.
    fn maybe_warn_size(&self, size: u64) {
        if size <= SIZE_WARN_BYTES {
            return;
        }
        let Some(tx) = &self.event_tx else {
            return;
        };
        if self
            .warned
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // 1024 bytes/KB; rough 4-bytes/token estimate matches the AC's
            // "25KB, ~6k tokens" ratio.
            let kb = size / 1024;
            let tok = size / 4 / 1000;
            // AC3 size/cost warning must surface as a SystemNotice, but a
            // MemoryPort adapter is given an `event_tx` at composition (mirroring
            // ToolSetAdapter::set_event_tx) and has no access to the event bus.
            let event = AppEvent::SystemNotice {
                conversation_id: None,
                level: NoticeLevel::Warning,
                message: format!(
                    "⚠ MEMORY.md is large ({kb}KB, ~{tok}k tokens). \
                     Consider pruning outdated entries to reduce context cost."
                ),
            };
            let _ = tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 11-2 AC3 — adapter event_tx, no event_bus access
        }
    }

    /// Parse `MEMORY.md` markdown into ordered `(category, facts)` sections.
    ///
    /// Format: an optional `# Title` H1 (skipped), then per topic a
    /// `## {Category}` H2, then `- {fact}` bullets; an indented non-empty line
    /// under a bullet is that fact's `detail`. The parser is tolerant of
    /// hand-edits — blank lines and unrecognized content are skipped rather than
    /// failing the load. Repeated `## {Category}` headings (case-insensitive)
    /// merge into the first occurrence, keeping the snapshot canonical.
    fn parse(content: &str) -> Vec<(String, Vec<MemoryFact>)> {
        let mut sections: Vec<(String, Vec<MemoryFact>)> = Vec::new();
        let mut cur: Option<usize> = None;

        for line in content.lines() {
            // `## ` headings start a (or resume a) category section. Checked
            // BEFORE the `# ` H1 case so it is never mistaken for a title.
            if let Some(cat) = line.strip_prefix("## ") {
                let cat = cat.trim();
                if cat.is_empty() {
                    continue;
                }
                cur = Some(
                    match sections
                        .iter()
                        .position(|(c, _)| c.eq_ignore_ascii_case(cat))
                    {
                        Some(i) => i,
                        None => {
                            sections.push((cat.to_string(), Vec::new()));
                            sections.len() - 1
                        }
                    },
                );
                continue;
            }
            // First-level `# ` heading → title; skip it.
            if line.starts_with("# ") {
                continue;
            }
            // `- ` bullet → a fact under the current section.
            if let Some(rest) = line.strip_prefix("- ") {
                let fact_text = rest.trim();
                if fact_text.is_empty() {
                    continue;
                }
                // Pre-heading bullets → assign to a default "Uncategorized"
                // section so human hand-edits are never silently dropped (AC2).
                if cur.is_none() {
                    let cat = "Uncategorized".to_string();
                    cur = match sections
                        .iter()
                        .position(|(c, _)| c.eq_ignore_ascii_case(&cat))
                    {
                        Some(i) => Some(i),
                        None => {
                            sections.push((cat, Vec::new()));
                            Some(sections.len() - 1)
                        }
                    };
                }
                if let Some(i) = cur {
                    let category = sections[i].0.clone();
                    sections[i].1.push(MemoryFact {
                        category,
                        fact: fact_text.to_string(),
                        detail: None,
                    });
                }
                continue;
            }
            // Indented continuation line → detail for the last fact. Blank
            // indented lines are preserved as paragraph breaks (round-trip
            // fidelity, DF-3 code review fix).
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(i) = cur {
                    if let Some(last) = sections[i].1.last_mut() {
                        let text = line.trim().to_string();
                        match &mut last.detail {
                            Some(d) => {
                                d.push('\n');
                                d.push_str(&text);
                            }
                            None => last.detail = Some(text),
                        }
                    }
                }
                continue;
            }
            // Blank / unrecognized line → skip defensively.
        }
        sections
    }

    /// Render the snapshot back to the canonical `MEMORY.md` markdown shape so
    /// that store → parse → store is stable. Empty sections are omitted.
    fn render(sections: &[(String, Vec<MemoryFact>)]) -> String {
        let mut buf = String::from("# MEMORY\n\n");
        for (category, facts) in sections {
            if facts.is_empty() {
                continue;
            }
            buf.push_str("## ");
            buf.push_str(category);
            buf.push_str("\n\n");
            for f in facts {
                buf.push_str("- ");
                buf.push_str(&f.fact);
                buf.push('\n');
                if let Some(detail) = &f.detail {
                    for dline in detail.lines() {
                        buf.push_str("  ");
                        buf.push_str(dline);
                        buf.push('\n');
                    }
                }
            }
            buf.push('\n');
        }
        buf
    }

    /// Timestamp used when mapping facts to `MemoryEntry` — the file mtime
    /// (long-term facts are all "current"; ordering is by section, and the
    /// composite enforces "long-term first" by concatenation, not timestamp).
    fn entry_timestamp(mtime: Option<SystemTime>) -> DateTime<Local> {
        match mtime {
            Some(t) => DateTime::<Local>::from(t),
            None => Local::now(),
        }
    }
}

#[async_trait]
impl MemoryPort for LongTermMemory {
    /// Upsert a durable fact into `MEMORY.md`, organized by topic (AC1). The
    /// whole file is rewritten (curated, NOT append-only). The fact is added
    /// only if it is not a normalized duplicate within its category. Rejects
    /// empty/whitespace text and sanitizes newline / `## `-heading injection
    /// (mirrors the 11.1 daily-log review patch).
    async fn remember_fact(&self, fact: MemoryFact) -> Result<(), MemoryError> {
        let category = fact.category.trim();
        let fact_text = fact.fact.trim();

        if fact_text.is_empty() {
            return Err(MemoryError::NotSupported("fact must not be empty".into()));
        }
        if category.is_empty() {
            return Err(MemoryError::NotSupported(
                "category must not be empty".into(),
            ));
        }
        // Phantom-entry / section-injection prevention (mirror daily_log 216–227).
        if category.contains('\n') || fact_text.contains('\n') {
            return Err(MemoryError::NotSupported(
                "category/fact must not contain newlines".into(),
            ));
        }
        if category.starts_with("## ") || fact_text.starts_with("## ") {
            return Err(MemoryError::NotSupported(
                "category/fact must not start with '## '".into(),
            ));
        }
        if let Some(ref d) = fact.detail {
            if d.trim().is_empty() {
                return Err(MemoryError::NotSupported(
                    "detail must not be empty or whitespace".into(),
                ));
            }
            if d.lines().any(|l| l.trim_start().starts_with("## ")) {
                return Err(MemoryError::NotSupported(
                    "detail must not contain lines starting with '## '".into(),
                ));
            }
        }

        // Pick up any manual edits before upserting (AC2 — don't clobber).
        self.ensure_fresh().await?;

        let category = category.to_string();
        let fact_text = fact_text.to_string();
        let detail = fact.detail.clone();
        let normalized_new = normalize(&fact_text);

        // Hold the write guard through the disk write so concurrent
        // `remember_fact` calls cannot render stale snapshots (P1 code-review
        // fix). `tokio::sync::RwLock` safely holds across `.await`.
        let mut guard = self.loaded.write().await;

        // TOCTOU defense (DF-1 code review fix): re-stat the file after
        // acquiring the write guard. If an external edit landed between
        // ensure_fresh and here, reload before upserting so we don't clobber
        // the human's changes.
        if guard.loaded_once {
            if let Ok(meta) = tokio::fs::metadata(&self.memory_file).await {
                let disk_mtime = meta.modified().ok();
                if disk_mtime != guard.mtime {
                    if let Ok(content) = tokio::fs::read_to_string(&self.memory_file).await {
                        guard.sections = Self::parse(&content);
                        guard.mtime = disk_mtime;
                    }
                }
            }
        }

        let sec_idx = match guard
            .sections
            .iter()
            .position(|(c, _)| c.eq_ignore_ascii_case(&category))
        {
            Some(i) => i,
            None => {
                guard.sections.push((category.clone(), Vec::new()));
                guard.sections.len() - 1
            }
        };

        // Upsert (P2 code-review fix): if a normalized duplicate exists,
        // update its detail when the new detail differs; otherwise insert.
        let existing = guard.sections[sec_idx]
            .1
            .iter_mut()
            .find(|f| normalize(&f.fact) == normalized_new);
        match existing {
            Some(f) => {
                if detail.is_some() && f.detail != detail {
                    f.detail = detail;
                }
            }
            None => {
                let canonical_cat = guard.sections[sec_idx].0.clone();
                guard.sections[sec_idx].1.push(MemoryFact {
                    category: canonical_cat,
                    fact: fact_text,
                    detail,
                });
            }
        }

        let rendered = Self::render(&guard.sections);

        // Test-only deterministic suspension seam (Story 12.0 AC10): pin the
        // interior TOCTOU window AFTER the DF-1 re-stat and BEFORE the
        // `tokio::fs::write`, while the `loaded` write guard is held. A profile
        // swap's `prepare_detach()` drain blocks behind this parked writer.
        #[cfg(test)]
        if self
            .seam
            .armed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.seam.reached.notify_one();
            self.seam.proceed.notified().await;
        }

        // Ensure `.rustain/` exists, then rewrite the whole file (curated).
        if let Some(parent) = self.memory_file.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                MemoryError::IoError(format!(
                    "failed to create memory dir {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }
        tokio::fs::write(&self.memory_file, rendered.as_bytes())
            .await
            .map_err(|e| {
                MemoryError::IoError(format!(
                    "failed to write {}: {}",
                    self.memory_file.display(),
                    e
                ))
            })?;

        let new_mtime = tokio::fs::metadata(&self.memory_file)
            .await
            .ok()
            .and_then(|m| m.modified().ok());
        guard.mtime = new_mtime;
        guard.loaded_once = true;
        drop(guard);
        Ok(())
    }

    /// Flatten all loaded facts to `MemoryEntry`s in file (section) order,
    /// capped at `limit`. Long-term facts are all "current"; the composite is
    /// what enforces "long-term first".
    async fn recent(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.ensure_fresh().await?;
        let guard = self.loaded.read().await;
        let ts = Self::entry_timestamp(guard.mtime);
        Ok(guard
            .sections
            .iter()
            .flat_map(|(_, facts)| facts.iter())
            .take(limit)
            .map(|f| MemoryEntry {
                timestamp: ts,
                summary: f.fact.clone(),
                context: f.detail.clone(),
            })
            .collect())
    }

    /// Case-insensitive substring match over `category` + `fact` + `detail`,
    /// in file order, capped at `limit`.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_fresh().await?;
        let needle = query.to_lowercase();
        let guard = self.loaded.read().await;
        let ts = Self::entry_timestamp(guard.mtime);
        Ok(guard
            .sections
            .iter()
            .flat_map(|(cat, facts)| facts.iter().map(move |f| (cat, f)))
            .filter(|(cat, f)| {
                cat.to_lowercase().contains(&needle)
                    || f.fact.to_lowercase().contains(&needle)
                    || f.detail
                        .as_ref()
                        .is_some_and(|d| d.to_lowercase().contains(&needle))
            })
            .take(limit)
            .map(|(_, f)| MemoryEntry {
                timestamp: ts,
                summary: f.fact.clone(),
                context: f.detail.clone(),
            })
            .collect())
    }

    /// Drain an in-flight curated upsert before a profile swap detaches this
    /// adapter (Story 12.0 C3 / AC3, AC9). `remember_fact` holds the `loaded`
    /// write guard across its whole read-modify-render-`fs::write` window, so
    /// acquiring (and dropping) that same guard here cannot return until the
    /// in-flight upsert has been written and the snapshot is consistent. The
    /// newly-composed adapter (sharing the same `MEMORY.md`) then `ensure_fresh`-
    /// reloads AFTER the drain, sees the just-written fact, and upserts on top of
    /// it — closing the lost-update window (non-monotonic MEMORY.md across a swap).
    async fn prepare_detach(
        &self,
    ) -> Result<crate::domain::models::TransitionState, crate::domain::errors::TransitionError>
    {
        let _drained = self.loaded.write().await;
        Ok(crate::domain::models::TransitionState::empty("memory"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(category: &str, fact: &str, detail: Option<&str>) -> MemoryFact {
        MemoryFact {
            category: category.to_string(),
            fact: fact.to_string(),
            detail: detail.map(|s| s.to_string()),
        }
    }

    // 1. remember_fact creates {tmp}/.rustain/MEMORY.md + the .rustain dir.
    #[tokio::test]
    async fn remember_fact_creates_file_and_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.remember_fact(fact("Preferences", "prefers snake_case", None))
            .await
            .unwrap();

        let dir = tmp.path().join(".rustain");
        assert!(dir.is_dir(), ".rustain dir auto-created");
        let file = dir.join("MEMORY.md");
        assert!(file.exists(), "MEMORY.md created");
        // It is a SIBLING of memory/, not inside it.
        assert!(!tmp.path().join(".rustain").join("memory").exists());
    }

    // 2. Topic organization (AC1): distinct categories → distinct sections; a
    //    second fact in an existing category appends under it (not a new one).
    #[tokio::test]
    async fn topic_organization() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.remember_fact(fact("Preferences", "prefers snake_case", None))
            .await
            .unwrap();
        mem.remember_fact(fact("Database", "PostgreSQL 15", None))
            .await
            .unwrap();
        mem.remember_fact(fact("Preferences", "tabs over spaces", None))
            .await
            .unwrap();

        let content =
            std::fs::read_to_string(tmp.path().join(".rustain").join("MEMORY.md")).unwrap();
        assert_eq!(
            content.matches("## Preferences").count(),
            1,
            "one Preferences section"
        );
        assert_eq!(
            content.matches("## Database").count(),
            1,
            "one Database section"
        );
        assert!(content.contains("- prefers snake_case"));
        assert!(content.contains("- tabs over spaces"));
        assert!(content.contains("- PostgreSQL 15"));
        // Both Preferences facts under the single section.
        let pref_idx = content.find("## Preferences").unwrap();
        let db_idx = content.find("## Database").unwrap();
        let pref_block = &content[pref_idx..db_idx.max(pref_idx)];
        // tabs-over-spaces was added last; ensure it appears (section append).
        assert!(content[pref_idx..].contains("tabs over spaces"));
        let _ = pref_block;
    }

    // 3. Dedup within category: same normalized fact twice → one bullet.
    #[tokio::test]
    async fn dedup_within_category() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.remember_fact(fact("Preferences", "prefers snake_case", None))
            .await
            .unwrap();
        // Different whitespace / case → normalizes to the same.
        mem.remember_fact(fact("preferences", "  Prefers   snake_case ", None))
            .await
            .unwrap();

        let content =
            std::fs::read_to_string(tmp.path().join(".rustain").join("MEMORY.md")).unwrap();
        assert_eq!(
            content.matches("snake_case").count(),
            1,
            "duplicate fact stored once"
        );
        assert_eq!(
            content.matches("## Preferences").count(),
            1,
            "single section, case-insensitive"
        );
    }

    // 4. Manual-edit reload (AC2): edit the file on disk → recent() reflects it.
    #[tokio::test]
    async fn manual_edit_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.remember_fact(fact("Database", "PostgreSQL 15", None))
            .await
            .unwrap();
        assert_eq!(mem.recent(10).await.unwrap().len(), 1);

        // Overwrite the file by hand with a new mtime — add + remove + update.
        let file = tmp.path().join(".rustain").join("MEMORY.md");
        // Ensure a strictly newer mtime than our write (filesystems can have
        // coarse mtime granularity).
        let new_mtime = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::write(
            &file,
            "# MEMORY\n\n## Database\n\n- MySQL 8\n  switched from postgres\n\n## Style\n\n- 4-space indent\n",
        )
        .unwrap();
        filetime_set(&file, new_mtime);

        let recent = mem.recent(10).await.unwrap();
        let summaries: Vec<&str> = recent.iter().map(|e| e.summary.as_str()).collect();
        assert!(
            summaries.contains(&"MySQL 8"),
            "added/updated fact picked up"
        );
        assert!(
            summaries.contains(&"4-space indent"),
            "new section picked up"
        );
        assert!(
            !summaries.contains(&"PostgreSQL 15"),
            "removed fact gone after reload"
        );
        // detail round-trips on reload.
        let mysql = recent.iter().find(|e| e.summary == "MySQL 8").unwrap();
        assert_eq!(mysql.context.as_deref(), Some("switched from postgres"));
    }

    // Helper: set a file's mtime (std-only; avoids a new dev-dep).
    fn filetime_set(path: &std::path::Path, when: std::time::SystemTime) {
        // Re-write with explicit mtime via the `utimensat`-free portable trick:
        // open + set_modified (Rust 1.75+ File::set_modified).
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }

    // 5. Empty state (AC5): fresh tempdir → initialize Ok, recent/search empty.
    #[tokio::test]
    async fn empty_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.initialize().await.unwrap();
        assert!(mem.recent(10).await.unwrap().is_empty());
        assert!(mem.search("anything", 10).await.unwrap().is_empty());
        // No file was created by a read-only path.
        assert!(!tmp.path().join(".rustain").join("MEMORY.md").exists());
    }

    // 6. Round-trip: render → parse → equal facts (category, fact, detail).
    #[tokio::test]
    async fn round_trip_render_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.remember_fact(fact(
            "Preferences",
            "prefers snake_case",
            Some("seen across files"),
        ))
        .await
        .unwrap();
        mem.remember_fact(fact("Database", "PostgreSQL 15", None))
            .await
            .unwrap();

        let content =
            std::fs::read_to_string(tmp.path().join(".rustain").join("MEMORY.md")).unwrap();
        let parsed = LongTermMemory::parse(&content);
        let reparsed = LongTermMemory::parse(&LongTermMemory::render(&parsed));
        assert_eq!(parsed, reparsed, "render→parse is stable");

        // Spot-check structure.
        assert_eq!(parsed.len(), 2);
        let pref = parsed.iter().find(|(c, _)| c == "Preferences").unwrap();
        assert_eq!(pref.1[0].fact, "prefers snake_case");
        assert_eq!(pref.1[0].detail.as_deref(), Some("seen across files"));
    }

    // 7. recent/search mapping: summary==fact, context==detail; search ci over
    //    category+fact+detail; cap respected.
    #[tokio::test]
    async fn recent_and_search_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.remember_fact(fact("Database", "PostgreSQL 15", Some("primary store")))
            .await
            .unwrap();
        mem.remember_fact(fact("Preferences", "prefers snake_case", None))
            .await
            .unwrap();

        let recent = mem.recent(10).await.unwrap();
        assert_eq!(recent.len(), 2);
        let pg = recent
            .iter()
            .find(|e| e.summary == "PostgreSQL 15")
            .unwrap();
        assert_eq!(pg.context.as_deref(), Some("primary store"));

        // Search over category.
        assert_eq!(mem.search("database", 10).await.unwrap().len(), 1);
        // Search over fact.
        assert_eq!(mem.search("snake", 10).await.unwrap().len(), 1);
        // Search over detail.
        assert_eq!(mem.search("primary store", 10).await.unwrap().len(), 1);
        // Case-insensitive.
        assert_eq!(mem.search("POSTGRESQL", 10).await.unwrap().len(), 1);
        // Cap respected.
        assert_eq!(mem.recent(1).await.unwrap().len(), 1);
        // No match.
        assert!(mem.search("nonexistent", 10).await.unwrap().is_empty());
    }

    // 8. Size warning (AC3): >20KB → exactly ONE Warning; a second load does
    //    NOT re-emit; a <20KB file emits nothing.
    #[tokio::test]
    async fn size_warning_once_over_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".rustain");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("MEMORY.md");

        // Build a >20KB MEMORY.md.
        let mut body = String::from("# MEMORY\n\n## Bulk\n\n");
        let mut n = 0;
        while body.len() < 25 * 1024 {
            body.push_str(&format!("- fact number {n} with some padding text here\n"));
            n += 1;
        }
        std::fs::write(&file, &body).unwrap();
        assert!(body.len() > SIZE_WARN_BYTES as usize);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut mem = LongTermMemory::new(tmp.path());
        mem.set_event_tx(tx);

        mem.initialize().await.unwrap();
        // Exactly one Warning notice.
        match rx.try_recv() {
            Ok(AppEvent::SystemNotice {
                level,
                message,
                conversation_id,
            }) => {
                assert_eq!(level, NoticeLevel::Warning);
                assert!(message.contains("MEMORY.md is large"));
                assert!(message.contains("k tokens"));
                assert!(conversation_id.is_none());
            }
            other => panic!("expected one SystemNotice Warning, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no second notice on first load");

        // A second access does NOT re-emit (once per session).
        let _ = mem.recent(5).await.unwrap();
        assert!(rx.try_recv().is_err(), "warning is once-per-session");
    }

    #[tokio::test]
    async fn no_size_warning_under_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut mem = LongTermMemory::new(tmp.path());
        mem.set_event_tx(tx);
        mem.remember_fact(fact("Preferences", "small file", None))
            .await
            .unwrap();
        mem.initialize().await.unwrap();
        assert!(rx.try_recv().is_err(), "small file emits no warning");
    }

    // 9. Reject empty fact / newline-in-fact / `## `-leading detail (sanitize).
    #[tokio::test]
    async fn reject_invalid_input() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());

        assert!(matches!(
            mem.remember_fact(fact("Cat", "   ", None)).await,
            Err(MemoryError::NotSupported(_))
        ));
        assert!(matches!(
            mem.remember_fact(fact("Cat", "line1\nline2", None)).await,
            Err(MemoryError::NotSupported(_))
        ));
        assert!(matches!(
            mem.remember_fact(fact("Cat", "fact", Some("ok\n## Injected heading")))
                .await,
            Err(MemoryError::NotSupported(_))
        ));
        assert!(matches!(
            mem.remember_fact(fact("  ", "valid fact", None)).await,
            Err(MemoryError::NotSupported(_))
        ));
        // None of the rejected writes created the file.
        assert!(!tmp.path().join(".rustain").join("MEMORY.md").exists());
    }

    // Inherited trait-default `store` is a no-op for the standalone adapter —
    // it must NOT route MemoryEntry appends into MEMORY.md.
    #[tokio::test]
    async fn store_is_noop_for_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.store(MemoryEntry {
            timestamp: Local::now(),
            summary: "operational record".into(),
            context: None,
        })
        .await
        .unwrap();
        assert!(
            mem.recent(10).await.unwrap().is_empty(),
            "store does not touch MEMORY.md"
        );
        assert!(!tmp.path().join(".rustain").join("MEMORY.md").exists());
    }

    // normalize: trims, collapses whitespace, lowercases.
    #[test]
    fn normalize_collapses_and_lowercases() {
        assert_eq!(normalize("  Prefers   snake_case "), "prefers snake_case");
        assert_eq!(normalize("A\tB  C"), "a b c");
    }

    // P2: Upsert detail — calling remember_fact with the same fact but new
    // detail updates the existing entry instead of discarding silently.
    #[tokio::test]
    async fn upsert_detail_on_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.remember_fact(fact("Database", "PostgreSQL 15", Some("primary store")))
            .await
            .unwrap();
        mem.remember_fact(fact("Database", "PostgreSQL 15", Some("migrated to 16")))
            .await
            .unwrap();

        let recent = mem.recent(10).await.unwrap();
        assert_eq!(recent.len(), 1, "still one fact (dedup)");
        assert_eq!(
            recent[0].context.as_deref(),
            Some("migrated to 16"),
            "detail updated"
        );

        let content =
            std::fs::read_to_string(tmp.path().join(".rustain").join("MEMORY.md")).unwrap();
        assert_eq!(content.matches("PostgreSQL 15").count(), 1);
    }

    // P4: Bullets before any ## heading are preserved under "Uncategorized".
    #[tokio::test]
    async fn pre_heading_bullets_preserved() {
        let content = "- orphan fact one\n- orphan fact two\n\n## Database\n\n- PostgreSQL 15\n";
        let parsed = LongTermMemory::parse(content);
        let uncategorized = parsed
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case("Uncategorized"))
            .expect("orphan bullets get an Uncategorized section");
        assert_eq!(uncategorized.1.len(), 2);
        assert_eq!(uncategorized.1[0].fact, "orphan fact one");
        assert_eq!(uncategorized.1[1].fact, "orphan fact two");

        let db = parsed
            .iter()
            .find(|(c, _)| c == "Database")
            .expect("Database section preserved");
        assert_eq!(db.1.len(), 1);
    }

    // P5: Empty-string detail is rejected at validation.
    #[tokio::test]
    async fn reject_empty_detail() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        assert!(matches!(
            mem.remember_fact(fact("Cat", "fact", Some("   "))).await,
            Err(MemoryError::NotSupported(_))
        ));
        assert!(matches!(
            mem.remember_fact(fact("Cat", "fact", Some(""))).await,
            Err(MemoryError::NotSupported(_))
        ));
        assert!(
            !tmp.path().join(".rustain").join("MEMORY.md").exists(),
            "no file created for rejected writes"
        );
    }

    // P3: Indented ## lines in detail are rejected (trim_start defense).
    #[tokio::test]
    async fn reject_indented_heading_in_detail() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        assert!(matches!(
            mem.remember_fact(fact("Cat", "fact", Some("  ## Injected Heading")))
                .await,
            Err(MemoryError::NotSupported(_))
        ));
    }

    // DF-3: Interior blank lines in detail are preserved through round-trip.
    #[tokio::test]
    async fn detail_blank_lines_preserved() {
        let content = "# MEMORY\n\n## Notes\n\n- some fact\n  line one\n  \n  line three\n";
        let parsed = LongTermMemory::parse(content);
        let fact = &parsed[0].1[0];
        let detail = fact.detail.as_ref().unwrap();
        assert!(detail.contains("\n\n"), "blank line preserved");
        assert!(detail.contains("line one"));
        assert!(detail.contains("line three"));

        let rendered = LongTermMemory::render(&parsed);
        let reparsed = LongTermMemory::parse(&rendered);
        assert_eq!(parsed, reparsed, "round-trip stable with blank lines");
    }

    // DF-5: search("") returns empty, not all entries.
    #[tokio::test]
    async fn search_empty_query_returns_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.remember_fact(fact("Cat", "some fact", None))
            .await
            .unwrap();
        assert!(mem.search("", 10).await.unwrap().is_empty());
        assert!(mem.search("   ", 10).await.unwrap().is_empty());
        assert_eq!(mem.search("some", 10).await.unwrap().len(), 1);
    }

    // DF-1: External manual edit during remember_fact is not clobbered.
    #[tokio::test]
    async fn external_edit_not_clobbered() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = LongTermMemory::new(tmp.path());
        mem.remember_fact(fact("Cat", "original", None))
            .await
            .unwrap();

        // Simulate external edit: rewrite file with new mtime.
        let file = tmp.path().join(".rustain").join("MEMORY.md");
        let new_mtime = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::write(
            &file,
            "# MEMORY\n\n## Cat\n\n- external edit\n  human-added detail\n",
        )
        .unwrap();
        filetime_set(&file, new_mtime);

        // remember_fact should pick up the external edit, not clobber it.
        mem.remember_fact(fact("Cat", "new fact", None))
            .await
            .unwrap();
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("external edit"), "external edit preserved");
        assert!(content.contains("new fact"), "new fact also written");
    }

    // ── Story 12.0 C2/C3 — profile-swap write safety (AC2, AC3, AC9, AC10) ──

    // C3 (AC3) — a profile swap straddling two `remember_fact` upserts to the
    // SAME category must NOT lose either fact, and MEMORY.md must stay monotonic
    // (curated count never goes backwards). Thread A's in-flight upsert parks at
    // the interior seam holding the `loaded` write guard; the swap's
    // `prepare_detach()` drain blocks behind it, so a freshly-composed adapter B
    // (same MEMORY.md) only writes AFTER A is durable and reloads A's fact first
    // → both survive. RED on the unfixed no-op `prepare_detach`: B composes and
    // writes its stale snapshot, then A's resumed write clobbers it (lost-update,
    // single fact remains). Deadlock-free under the drain fix; the unfixed
    // lost-update was confirmed by reverting `prepare_detach` (see story Debug
    // Log). No `sleep`.
    #[tokio::test]
    async fn prepare_detach_prevents_lost_update_across_swap() {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let mem_a = Arc::new(LongTermMemory::new(tmp.path()));
        mem_a.seam.armed.store(true, Ordering::SeqCst);

        // Thread A — in-flight upsert, parks mid-write holding the write guard.
        let a = Arc::clone(&mem_a);
        let a_handle = tokio::spawn(async move {
            a.remember_fact(fact("Shared", "fact from A", None))
                .await
                .unwrap();
        });
        mem_a.seam.reached.notified().await;

        // The warm swap: drain A, THEN compose adapter B over the SAME file and
        // upsert a second fact to the same category.
        let root = tmp.path().to_path_buf();
        let mem_a2 = Arc::clone(&mem_a);
        let swap = tokio::spawn(async move {
            mem_a2.prepare_detach().await.unwrap();
            let mem_b = LongTermMemory::new(&root);
            mem_b
                .remember_fact(fact("Shared", "fact from B", None))
                .await
                .unwrap();
        });

        // Release A so its parked write completes and the drain can finish.
        mem_a.seam.proceed.notify_one();
        swap.await.unwrap();
        a_handle.await.unwrap();

        let content =
            std::fs::read_to_string(tmp.path().join(".rustain").join("MEMORY.md")).unwrap();
        assert!(
            content.contains("fact from A"),
            "A's fact survives the swap (no lost write)"
        );
        assert!(
            content.contains("fact from B"),
            "B's fact present (no lost update)"
        );
        assert_eq!(
            content.matches("## Shared").count(),
            1,
            "single canonical category section after the swap"
        );
    }
}
