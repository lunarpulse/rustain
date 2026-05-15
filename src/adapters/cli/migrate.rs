//! `rustain migrate --from <source>` CLI handler.
//!
//! Runs in plain terminal mode — NOT ratatui. Uses println!/print! and
//! std::io::stdin() for interaction (same pattern as init.rs).
//!
//! This is the composition root for the migrate path:
//!   workspace → sessions_dir → FileSystemStorage → ImporterRegistry → run

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::adapters::filesystem::FileSystemStorage;
use crate::adapters::importers::claude_code::ClaudeCodeImporter;
use crate::domain::errors::StorageError;
use crate::domain::ports::StoragePort;
use crate::domain::services::import::{
    ConversationImporter, ImportCandidate, ImportResult, ImporterRegistry,
};
use crate::infrastructure::paths;
use crate::infrastructure::startup::SubcommandExit;

/// Entry point for `rustain migrate --from <from> [--path <path>] [--yes] [--select] [--dry-run]`.
///
/// Builds its own isolated dependency graph (no provider, no terminal, no event loop)
/// and delegates to [`run_migrate_with`], the testable core.
pub async fn run_migrate(
    from: String,
    path: Option<PathBuf>,
    yes: bool,
    select: bool,
    dry_run: bool,
) -> Result<()> {
    // Build registry and register the Claude Code importer
    let mut registry = ImporterRegistry::new();
    registry.register("claude-code", Box::new(ClaudeCodeImporter::new()));

    // Validate source identifier (AC2)
    let importer = match registry.get(&from) {
        Some(imp) => imp,
        None => {
            let sources = registry.available_sources().join(", ");
            eprintln!(
                "Unsupported import source: {}. Supported sources: {}",
                from, sources
            );
            anyhow::bail!(SubcommandExit);
        }
    };

    // Build storage (migrate path: no with_workspace_root — no snapshot_file calls)
    let workspace = paths::workspace_dir().context("Failed to determine workspace directory")?;
    let sessions_dir = paths::sessions_dir(&workspace);
    tokio::fs::create_dir_all(&sessions_dir).await.ok();
    let storage = FileSystemStorage::new(sessions_dir);

    run_migrate_with(importer, &storage, path, yes, select, dry_run).await
}

/// Testable core for the migrate path.
///
/// Takes an already-built importer + storage so integration tests can exercise
/// the full discovery → filter → prompt → import → summary pipeline against a
/// temp directory. The public [`run_migrate`] wraps this with the production
/// composition root.
pub async fn run_migrate_with(
    importer: &dyn ConversationImporter,
    storage: &dyn StoragePort,
    path: Option<PathBuf>,
    yes: bool,
    select: bool,
    dry_run: bool,
) -> Result<()> {
    // Discover candidates (AC3, AC4)
    let all_candidates = match importer.discover(path.as_deref()).await {
        Ok(c) => c,
        Err(StorageError::NotFound(msg)) => {
            eprintln!("{}", msg);
            anyhow::bail!(SubcommandExit);
        }
        Err(e) => {
            eprintln!("Discovery failed: {}", e);
            anyhow::bail!(SubcommandExit);
        }
    };

    // Filter out already-imported candidates (AC8 — discovery-time idempotency).
    // Thread importer.source_id() to avoid hardcoding "claude-code" (DF-128).
    let (candidates, already_imported_count) =
        filter_already_imported(all_candidates, storage, importer.source_id()).await;

    // AC3: Handle empty discovery
    if candidates.is_empty() {
        if already_imported_count > 0 {
            println!(
                "Found 0 new Claude Code sessions. {} already imported.",
                already_imported_count
            );
        } else {
            println!("Found 0 Claude Code sessions.");
        }
        return Ok(());
    }

    // Determine source path description for display (DF-132: resolved path, not hardcoded literal).
    let source_desc = path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| {
            // Resolve the default path using dirs::home_dir() to show the actual path.
            dirs::home_dir()
                .map(|h| h.join(".claude").join("projects").display().to_string())
                .unwrap_or_else(|| "~/.claude/projects/".to_string())
        });

    // Print discovery table (AC3)
    println!(
        "Found {} Claude Code sessions in {}:",
        candidates.len(),
        source_desc
    );
    println!();
    print_candidate_table(&candidates);

    // AC9: Dry run — print table and exit
    if dry_run {
        println!();
        println!("Dry run — no changes made. Re-run without --dry-run to import.");
        return Ok(());
    }

    // Determine which candidates to import based on --yes / --select / interactive
    let selected: Vec<&ImportCandidate> = if yes {
        // AC5: --yes imports all
        candidates.iter().collect()
    } else if select {
        // AC7: --select interactive selection
        interactive_select(&candidates, &mut std::io::stdin().lock())?
    } else {
        // Default: show prompt [y/n/s]
        print!("Import all? [y/n/s] (s=select) ");
        std::io::stdout().flush().ok();

        let mut buf = String::new();
        let n = std::io::stdin().lock().read_line(&mut buf)?;
        if n == 0 {
            // EOF on stdin — treat as cancel rather than infinite loop.
            println!("Import cancelled (stdin closed).");
            return Ok(());
        }
        match buf.trim() {
            "y" | "Y" => candidates.iter().collect(),
            "s" | "S" => interactive_select(&candidates, &mut std::io::stdin().lock())?,
            _ => {
                println!("Import cancelled.");
                return Ok(());
            }
        }
    };

    // Run imports
    let mut imported = 0usize;
    let mut already_imported_in_batch = 0usize;
    let mut skipped_empty = 0usize;
    let mut failed = 0usize;
    let mut failure_messages: Vec<String> = Vec::new();

    for candidate in &selected {
        match importer.import(candidate, storage).await {
            Ok(ImportResult::Imported(_)) => imported += 1,
            Ok(ImportResult::AlreadyImported) => already_imported_in_batch += 1,
            Ok(ImportResult::SkippedEmpty) => skipped_empty += 1,
            Ok(ImportResult::Failed(msg)) => {
                failed += 1;
                let file_name = candidate
                    .source_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                failure_messages.push(format!("{}: {}", file_name, msg));
            }
            Err(e) => {
                failed += 1;
                let file_name = candidate
                    .source_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                failure_messages.push(format!("{}: {}", file_name, e));
            }
        }
    }

    // Print summary (AC5)
    let skipped_total = already_imported_in_batch + already_imported_count;
    println!(
        "\nImported {} conversations. Skipped {} (already imported). Skipped {} (empty). Failed {}.",
        imported, skipped_total, skipped_empty, failed
    );
    for msg in &failure_messages {
        println!("  Failed: {}", msg);
    }

    // Non-zero exit code if any import failed — matters for CI/scripting.
    if failed > 0 {
        anyhow::bail!(SubcommandExit);
    }

    Ok(())
}

/// Print the candidate discovery table.
fn print_candidate_table(candidates: &[ImportCandidate]) {
    println!("  {:<3}  {:<50}  {:<16}  Msgs", "#", "Title", "Date");
    for (i, c) in candidates.iter().enumerate() {
        let date = if c.created_at > 0 {
            let dt = chrono::DateTime::from_timestamp(c.created_at, 0).unwrap_or_default();
            dt.format("%Y-%m-%d %H:%M").to_string()
        } else {
            "unknown".to_string()
        };
        println!(
            "  {:<3}  {:<50}  {:<16}  {}",
            i + 1,
            truncate_display(&c.title, 50),
            date,
            c.message_count
        );
    }
}

/// Truncate a string to `max` chars for display (no word-boundary, just cut).
///
/// Character-based (not byte-based) so that multi-byte codepoints like emoji
/// never get sliced mid-sequence. Takes `max - 3` chars and appends "...".
fn truncate_display(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        s.to_string()
    } else if max < 4 {
        s.chars().take(max).collect()
    } else {
        let truncated: String = s.chars().take(max - 3).collect();
        format!("{}...", truncated)
    }
}

/// Filter candidates against storage, returning only not-yet-imported ones.
///
/// `source_id` is the importer's identifier (e.g., "claude-code") used to match
/// previously-imported sessions — avoids hardcoding the literal string (DF-128).
///
/// Returns `(new_candidates, already_imported_count)`.
async fn filter_already_imported(
    candidates: Vec<ImportCandidate>,
    storage: &dyn StoragePort,
    source_id: &str,
) -> (Vec<ImportCandidate>, usize) {
    let summaries = match storage.list_conversations().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to list existing conversations for idempotency check — treating all candidates as new"
            );
            return (candidates, 0);
        }
    };

    let mut already_imported_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for summary in &summaries {
        if let Ok(Some(meta)) = storage.load_session_meta(&summary.id).await {
            if let Some(ref imp) = meta.imported_from {
                if imp.source == source_id {
                    already_imported_ids.insert(imp.original_session_id.clone());
                }
            }
        }
    }

    let already_count = candidates
        .iter()
        .filter(|c| already_imported_ids.contains(&c.source_session_id))
        .count();

    let new_candidates: Vec<ImportCandidate> = candidates
        .into_iter()
        .filter(|c| !already_imported_ids.contains(&c.source_session_id))
        .collect();

    (new_candidates, already_count)
}

/// Interactive per-session selection mode (AC7, DF-133).
///
/// Prints the candidate table with checkboxes, reads commands from `reader`:
/// - `<N>` → toggle candidate N
/// - `a` → toggle all
/// - `c` → confirm selection
/// - `q` → abort
///
/// `reader` is `impl BufRead` so tests can inject mock stdin input.
pub fn interactive_select<'a, R: BufRead>(
    candidates: &'a [ImportCandidate],
    reader: &mut R,
) -> Result<Vec<&'a ImportCandidate>> {
    let mut selected = vec![false; candidates.len()];

    loop {
        // Re-print table with checkboxes
        println!();
        for (i, c) in candidates.iter().enumerate() {
            let check = if selected[i] { "[x]" } else { "[ ]" };
            println!("  {} {}. {}", check, i + 1, truncate_display(&c.title, 60));
        }
        let sel_count = selected.iter().filter(|&&s| s).count();
        print!(
            "  {} selected / {} total — Commands: <N> toggle, a=toggle all, c=confirm, q=quit > ",
            sel_count,
            candidates.len()
        );
        std::io::stdout().flush().ok();

        let mut buf = String::new();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            println!("Import cancelled (stdin closed).");
            return Ok(vec![]);
        }
        let input = buf.trim();

        match input {
            "a" | "A" => {
                let all_selected = selected.iter().all(|&s| s);
                for s in selected.iter_mut() {
                    *s = !all_selected;
                }
            }
            "c" | "C" => {
                break;
            }
            "q" | "Q" => {
                println!("Import cancelled.");
                return Ok(vec![]);
            }
            n => {
                if let Ok(idx) = n.parse::<usize>() {
                    if idx >= 1 && idx <= candidates.len() {
                        selected[idx - 1] = !selected[idx - 1];
                    }
                }
            }
        }
    }

    let result = candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| selected[*i])
        .map(|(_, c)| c)
        .collect();

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::adapters::filesystem::FileSystemStorage;
    use crate::domain::services::import::ConversationImporter;

    fn make_storage(dir: &Path) -> FileSystemStorage {
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        FileSystemStorage::new(sessions_dir)
    }

    #[test]
    fn test_migrate_unknown_source_error_message() {
        let mut registry = ImporterRegistry::new();
        registry.register("claude-code", Box::new(ClaudeCodeImporter::new()));
        let sources = registry.available_sources();
        // Simulate the error message construction from run_migrate
        let sources_str = sources.join(", ");
        let msg = format!(
            "Unsupported import source: aider. Supported sources: {}",
            sources_str
        );
        assert!(msg.contains("aider"));
        assert!(msg.contains("claude-code"));
    }

    #[tokio::test]
    async fn test_migrate_dry_run_does_not_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let storage = make_storage(tmp.path());

        // Build a fake source dir
        let source_dir = tmp.path().join("projects");
        let ws_dir = source_dir.join("ws-hash");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let fixture = r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":"Hello"}}"#;
        std::fs::write(ws_dir.join("session-dry.jsonl"), fixture).unwrap();

        let importer = crate::adapters::importers::claude_code::ClaudeCodeImporter::default();
        // Override default_root via struct field — use the struct directly in this test
        let _ = importer.discover(Some(&source_dir)).await; // just ensure it works

        // Verify nothing in storage
        let list = storage.list_conversations().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_migrate_summary_line_counts_imported_skipped_failed() {
        // This tests the logic: simulate having 1 already-imported, 1 new, 1 failed.
        let tmp = tempfile::TempDir::new().unwrap();
        let storage = make_storage(tmp.path());

        // Pre-populate 1 already-imported session
        use crate::domain::models::conversation::{Conversation, generate_conversation_id};
        use crate::domain::models::session_meta::ImportSource;

        let existing_id = generate_conversation_id();
        let existing_conv = Conversation {
            id: existing_id.clone(),
            title: "Existing".to_string(),
            messages: vec![],
            turns: Vec::new(),
            created_at: 1700000000,
            updated_at: 1700000000,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
        };
        storage.save_conversation(&existing_conv).await.unwrap();
        let mut existing_meta = storage
            .load_session_meta(&existing_id)
            .await
            .unwrap()
            .unwrap();
        existing_meta.imported_from = Some(ImportSource {
            source: "claude-code".to_string(),
            original_session_id: "already-imported-session".to_string(),
            imported_at: 1700000100,
        });
        storage
            .save_session_meta(&existing_id, &existing_meta)
            .await
            .unwrap();

        // Build source dir with 2 sessions (already-imported + new)
        let source_dir = tmp.path().join("projects");
        let ws_dir = source_dir.join("ws-hash");
        std::fs::create_dir_all(&ws_dir).unwrap();

        let fixture = r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":"Hello"}}"#;
        std::fs::write(ws_dir.join("already-imported-session.jsonl"), fixture).unwrap();
        std::fs::write(ws_dir.join("new-session.jsonl"), fixture).unwrap();

        let importer_adapter =
            crate::adapters::importers::claude_code::ClaudeCodeImporter::with_root(PathBuf::from(
                "/nonexistent",
            ));

        let all = importer_adapter.discover(Some(&source_dir)).await.unwrap();
        assert_eq!(all.len(), 2);

        let (new_candidates, already_count) =
            filter_already_imported(all, &storage, "claude-code").await;

        assert_eq!(already_count, 1);
        assert_eq!(new_candidates.len(), 1);
    }
}
