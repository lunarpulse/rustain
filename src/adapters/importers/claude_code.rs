//! Claude Code → rustain session importer.
//!
//! Outbound adapter implementing `ConversationImporter` for Claude Code's
//! `~/.claude/projects/{workspace_hash}/*.jsonl` format.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::domain::errors::StorageError;
use crate::domain::models::conversation::{Conversation, generate_conversation_id};
use crate::domain::models::session_meta::{ImportSource, now_unix};
use crate::domain::ports::StoragePort;
use crate::domain::services::claude_code_jsonl::{
    convert_lines_to_chat_messages, extract_candidate_metadata, parse_jsonl_line,
};
use crate::domain::services::import::{ConversationImporter, ImportCandidate, ImportResult};

/// Imports Claude Code sessions from `~/.claude/projects/`.
///
/// Directory layout: `{root}/{workspace_hash}/{session_uuid}.jsonl`
/// Two levels: the workspace-hash directory is the first level, the `.jsonl`
/// files are at the second level.
pub struct ClaudeCodeImporter {
    default_root: PathBuf,
}

impl ClaudeCodeImporter {
    /// Create a new importer.
    ///
    /// Default root is `~/.claude/projects/`. If the home directory cannot be
    /// determined, falls back to `~/.claude/projects/` literally (will fail at
    /// runtime with a `NotFound` error).
    pub fn new() -> Self {
        let default_root = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".claude")
            .join("projects");
        Self { default_root }
    }
}

impl Default for ClaudeCodeImporter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCodeImporter {
    // NOTE: `new()` is defined in the first `impl` block above.
    // This second block adds the test/override constructor.

    /// Create an importer using the given directory as the source root.
    /// Useful for tests and for `--path` overrides (though `--path` is passed
    /// directly to `discover(path)`, not set here).
    #[allow(dead_code)]
    pub fn with_root(root: PathBuf) -> Self {
        Self { default_root: root }
    }
}

#[async_trait]
impl ConversationImporter for ClaudeCodeImporter {
    fn source_name(&self) -> &'static str {
        "Claude Code"
    }

    fn source_id(&self) -> &'static str {
        "claude-code"
    }

    async fn discover(&self, path: Option<&Path>) -> Result<Vec<ImportCandidate>, StorageError> {
        let root = path.unwrap_or(self.default_root.as_path());

        // Verify the root path exists and is a directory
        match tokio::fs::metadata(root).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(StorageError::NotFound(format!(
                    "Path is not a directory: {}. --path must point to a directory containing workspace subdirectories.",
                    root.display()
                )));
            }
            Err(_) => {
                return Err(StorageError::NotFound(format!(
                    "Claude Code session directory not found at {}. Specify path with --path <dir>.",
                    root.display()
                )));
            }
        }

        // Walk root/*/  (workspace-hash subdirectories)
        let mut dir_reader = tokio::fs::read_dir(root).await.map_err(|e| {
            StorageError::IoError(format!("Failed to read Claude Code projects dir: {}", e))
        })?;

        let mut candidates: Vec<ImportCandidate> = Vec::new();

        loop {
            let workspace_entry = match dir_reader.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("Error iterating projects dir, skipping entry: {}", e);
                    continue;
                }
            };
            let workspace_path = workspace_entry.path();
            let workspace_meta = match tokio::fs::metadata(&workspace_path).await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !workspace_meta.is_dir() {
                continue;
            }

            // Read .jsonl files in this workspace directory
            let mut workspace_reader = match tokio::fs::read_dir(&workspace_path).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Failed to read workspace dir {:?}: {}", workspace_path, e);
                    continue;
                }
            };

            loop {
                let file_entry = match workspace_reader.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(
                            "Error iterating workspace dir {:?}, skipping entry: {}",
                            workspace_path,
                            e
                        );
                        continue;
                    }
                };
                let file_path = file_entry.path();
                if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }

                // DF-138: per-file size limit (~32 MiB) — skip oversized JSONL files.
                const MAX_JSONL_BYTES: u64 = 32 * 1024 * 1024;
                match tokio::fs::metadata(&file_path).await {
                    Ok(m) if m.len() > MAX_JSONL_BYTES => {
                        tracing::warn!(
                            path = %file_path.display(),
                            size_bytes = m.len(),
                            "Skipping oversized JSONL file during discovery (>{} MiB)",
                            MAX_JSONL_BYTES / (1024 * 1024)
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("Failed to stat {:?}: {} — skipping", file_path, e);
                        continue;
                    }
                    Ok(_) => {}
                }

                let contents = match tokio::fs::read_to_string(&file_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("Failed to read {:?}: {} — skipping", file_path, e);
                        continue;
                    }
                };

                match extract_candidate_metadata(&file_path, &contents) {
                    Ok(candidate) => candidates.push(candidate),
                    Err(e) => {
                        tracing::warn!(
                            "Failed to extract metadata from {:?}: {} — skipping",
                            file_path,
                            e
                        );
                    }
                }
            }
        }

        // Sort by created_at ascending (oldest first — stable numbering)
        candidates.sort_by_key(|c| c.created_at);
        Ok(candidates)
    }

    async fn import(
        &self,
        candidate: &ImportCandidate,
        storage: &dyn StoragePort,
    ) -> Result<ImportResult, StorageError> {
        // --- Idempotency check (AC8) ---
        if is_already_imported(candidate, storage, self.source_id()).await? {
            return Ok(ImportResult::AlreadyImported);
        }

        // Read + parse the source file
        let contents = match tokio::fs::read_to_string(&candidate.source_path).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ImportResult::Failed(format!(
                    "Failed to read source file: {}",
                    e
                )));
            }
        };

        // Parse all lines
        let mut lines = Vec::new();
        for raw_line in contents.lines() {
            match parse_jsonl_line(raw_line) {
                Ok(Some(line)) => lines.push(line),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        "Skipping malformed line in {:?}: {}",
                        candidate.source_path,
                        e
                    );
                }
            }
        }

        // Convert to ChatMessages
        let messages = convert_lines_to_chat_messages(&lines);

        // Skip empty imports — report as SkippedEmpty per AC5.
        if messages.is_empty() {
            return Ok(ImportResult::SkippedEmpty);
        }

        // Compute timestamps for the Conversation
        let created_at = lines
            .iter()
            .find_map(|l| {
                l.timestamp
                    .as_deref()
                    .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                    .map(|dt| dt.timestamp())
            })
            .unwrap_or(0);
        let updated_at = lines
            .iter()
            .rev()
            .find_map(|l| {
                l.timestamp
                    .as_deref()
                    .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                    .map(|dt| dt.timestamp())
            })
            .unwrap_or(created_at);

        // Build conversation
        let new_id = generate_conversation_id();
        let conv = Conversation {
            id: new_id.clone(),
            title: candidate.title.clone(),
            messages,
            created_at,
            updated_at,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        };

        // Write conversation + initial SessionMeta (imported_from: None)
        storage.save_conversation(&conv).await?;

        // Load the freshly-written SessionMeta, tag it with import provenance,
        // and re-save (two-pass write — see Dev Notes § SessionMeta Atomic Write Safety).
        //
        // Rollback on meta-write failure: if the second write fails after the
        // first succeeded, the saved conversation would be tagged as native —
        // next re-import would duplicate it. Delete the orphan and propagate
        // the error so the caller reports it in the summary.
        let meta_opt = match storage.load_session_meta(&new_id).await {
            Ok(m) => m,
            Err(e) => {
                if let Err(cleanup_err) = storage.delete_conversation(&new_id).await {
                    tracing::warn!(
                        orphan_id = %new_id,
                        cleanup_err = %cleanup_err,
                        "Failed to roll back orphan conversation after meta-load failure"
                    );
                }
                return Err(e);
            }
        };
        match meta_opt {
            Some(mut meta) => {
                meta.imported_from = Some(ImportSource {
                    source: self.source_id().to_string(), // DF-128: use source_id(), not literal
                    original_session_id: candidate.source_session_id.clone(),
                    imported_at: now_unix(),
                });
                if let Err(e) = storage.save_session_meta(&new_id, &meta).await {
                    if let Err(cleanup_err) = storage.delete_conversation(&new_id).await {
                        tracing::warn!(
                            orphan_id = %new_id,
                            cleanup_err = %cleanup_err,
                            "Failed to roll back orphan conversation after meta-write failure"
                        );
                    }
                    return Err(e);
                }
            }
            None => {
                tracing::warn!(
                    "SessionMeta not found after save_conversation for {} — rolling back to prevent untagged orphan",
                    new_id
                );
                if let Err(cleanup_err) = storage.delete_conversation(&new_id).await {
                    tracing::warn!(
                        orphan_id = %new_id,
                        cleanup_err = %cleanup_err,
                        "Failed to roll back orphan conversation after missing meta"
                    );
                }
                return Ok(ImportResult::Failed(format!(
                    "SessionMeta not found after save for {} — conversation rolled back",
                    new_id
                )));
            }
        }

        Ok(ImportResult::Imported(new_id))
    }
}

/// Check if a candidate has already been imported into `storage`.
///
/// `source_id` is the importer's identifier (e.g., "claude-code"). Using the
/// importer's `source_id()` prevents hardcoding (DF-129).
///
/// O(N × M): acceptable for v1 one-shot migration. See AC8 performance note.
async fn is_already_imported(
    candidate: &ImportCandidate,
    storage: &dyn StoragePort,
    source_id: &str,
) -> Result<bool, StorageError> {
    let summaries = storage.list_conversations().await?;
    for summary in &summaries {
        match storage.load_session_meta(&summary.id).await {
            Ok(Some(meta)) => {
                if let Some(ref imp) = meta.imported_from {
                    if imp.source == source_id
                        && imp.original_session_id == candidate.source_session_id
                    {
                        return Ok(true);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    session_id = %summary.id,
                    error = %e,
                    "Failed to load session meta during idempotency check — skipping session"
                );
            }
        }
    }
    Ok(false)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::domain::ports::StoragePort;

    fn make_importer_for_dir(root: PathBuf) -> ClaudeCodeImporter {
        ClaudeCodeImporter::with_root(root)
    }

    fn write_fixture(dir: &Path, workspace: &str, session_id: &str, lines: &[&str]) -> PathBuf {
        let ws_dir = dir.join(workspace);
        std::fs::create_dir_all(&ws_dir).unwrap();
        let file_path = ws_dir.join(format!("{}.jsonl", session_id));
        std::fs::write(&file_path, lines.join("\n")).unwrap();
        file_path
    }

    fn make_storage(dir: &Path) -> FileSystemStorage {
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        FileSystemStorage::new(sessions_dir)
    }

    const FIXTURE_USER_LINE: &str = r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":"Hello world"}}"#;
    const FIXTURE_ASST_LINE: &str = r#"{"type":"assistant","uuid":"a1","timestamp":"2026-04-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi there"}]}}"#;

    #[tokio::test]
    async fn test_discover_returns_error_when_directory_missing() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nonexistent");
        let importer = make_importer_for_dir(missing.clone());
        let result = importer.discover(Some(&missing)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found") || err.to_string().contains("Not found"));
    }

    #[tokio::test]
    async fn test_discover_returns_empty_when_directory_has_no_jsonl() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(root.join("workspace-hash")).unwrap();
        let importer = make_importer_for_dir(root.clone());
        let candidates = importer.discover(Some(&root)).await.unwrap();
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_discover_parses_fixture_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        write_fixture(
            &root,
            "ws-hash",
            "session-abc",
            &[FIXTURE_USER_LINE, FIXTURE_ASST_LINE],
        );
        let importer = make_importer_for_dir(root.clone());
        let candidates = importer.discover(Some(&root)).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].source_session_id, "session-abc");
        assert_eq!(candidates[0].title, "Hello world");
        assert_eq!(candidates[0].message_count, 2);
    }

    #[tokio::test]
    async fn test_discover_sorts_by_created_at_ascending() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        // session2 has earlier timestamp
        write_fixture(
            &root,
            "ws-hash",
            "session-late",
            &[
                r#"{"type":"user","uuid":"u1","timestamp":"2026-04-02T10:00:00Z","message":{"role":"user","content":"Later session"}}"#,
            ],
        );
        write_fixture(
            &root,
            "ws-hash2",
            "session-early",
            &[
                r#"{"type":"user","uuid":"u2","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":"Earlier session"}}"#,
            ],
        );
        let importer = make_importer_for_dir(root.clone());
        let candidates = importer.discover(Some(&root)).await.unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].source_session_id, "session-early");
        assert_eq!(candidates[1].source_session_id, "session-late");
    }

    #[tokio::test]
    async fn test_import_writes_conversation_and_session_meta_with_imported_from() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        let session_id = "session-import-test";
        write_fixture(
            &root,
            "ws-hash",
            session_id,
            &[FIXTURE_USER_LINE, FIXTURE_ASST_LINE],
        );
        let importer = make_importer_for_dir(root.clone());
        let storage = make_storage(tmp.path());

        let candidates = importer.discover(Some(&root)).await.unwrap();
        assert_eq!(candidates.len(), 1);

        let result = importer.import(&candidates[0], &storage).await.unwrap();
        let new_id = match result {
            ImportResult::Imported(id) => id,
            other => panic!("Expected Imported, got {:?}", other),
        };

        // Verify conversation exists
        let conv = storage.load_conversation(&new_id).await.unwrap();
        assert!(conv.is_some());
        assert_eq!(conv.unwrap().messages.len(), 2);

        // Verify imported_from is set
        let meta = storage.load_session_meta(&new_id).await.unwrap().unwrap();
        let imp = meta
            .imported_from
            .as_ref()
            .expect("imported_from must be set");
        assert_eq!(imp.source, "claude-code");
        assert_eq!(imp.original_session_id, session_id);
        assert!(imp.imported_at > 0);
    }

    #[tokio::test]
    async fn test_import_second_call_returns_already_imported() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        write_fixture(
            &root,
            "ws-hash",
            "session-idem",
            &[FIXTURE_USER_LINE, FIXTURE_ASST_LINE],
        );
        let importer = make_importer_for_dir(root.clone());
        let storage = make_storage(tmp.path());

        let candidates = importer.discover(Some(&root)).await.unwrap();

        // First import
        let r1 = importer.import(&candidates[0], &storage).await.unwrap();
        assert!(matches!(r1, ImportResult::Imported(_)));

        // Second import on same candidate — should be AlreadyImported
        let r2 = importer.import(&candidates[0], &storage).await.unwrap();
        assert!(matches!(r2, ImportResult::AlreadyImported));

        // Storage should still have exactly 1 conversation
        let list = storage.list_conversations().await.unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn test_import_skips_empty_conversion_result() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        // All system/meta lines — no real messages
        write_fixture(
            &root,
            "ws-hash",
            "session-empty",
            &[
                r#"{"type":"system","timestamp":"2026-04-01T10:00:00Z"}"#,
                r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:01:00Z","isMeta":true,"message":{"role":"user","content":"injected"}}"#,
            ],
        );
        let importer = make_importer_for_dir(root.clone());
        let storage = make_storage(tmp.path());

        let candidates = importer.discover(Some(&root)).await.unwrap();
        assert_eq!(candidates.len(), 1);

        let result = importer.import(&candidates[0], &storage).await.unwrap();
        assert!(matches!(result, ImportResult::SkippedEmpty));

        // Nothing written
        let list = storage.list_conversations().await.unwrap();
        assert!(list.is_empty());
    }
}
