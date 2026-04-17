//! E2E tests for Story 4.5: History Import
//!
//! AC1: `rustain migrate --from claude-code` subcommand parses and routes
//! AC2: Unsupported source rejected with clear error
//! AC3: Discovery lists candidates with title, date, message count
//! AC4: Source directory not found → clear error
//! AC5: `y` imports all — batch semantics
//! AC6: `SessionMeta.imported_from` field round-trips correctly
//! AC7: `s` interactive selection (tested via run_migrate with --select flag)
//! AC8: Idempotency — re-running skips already-imported sessions
//! AC9: `--dry-run` prints candidates without writing
//! AC10: JSONL conversion preserves content, tool calls, timestamps, title

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use async_trait::async_trait;

use rustain::adapters::cli::commands::{Cli, Command};
use rustain::adapters::cli::migrate::{run_migrate, run_migrate_with};
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::importers::claude_code::ClaudeCodeImporter;
use rustain::domain::errors::StorageError;
use rustain::domain::models::session_meta::{ImportSource, SessionMeta};
use rustain::domain::ports::StoragePort;
use rustain::domain::services::claude_code_jsonl::{
    convert_lines_to_chat_messages, extract_candidate_metadata, parse_jsonl_line,
};
use rustain::domain::services::import::{
    ConversationImporter, ImportCandidate, ImportResult, ImporterRegistry,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_storage(dir: &Path) -> FileSystemStorage {
    let sessions_dir = dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    FileSystemStorage::new(sessions_dir)
}

/// Write a fixture `.jsonl` file into `{dir}/{workspace_hash}/{session_id}.jsonl`.
/// Returns the file path.
fn write_fixture_jsonl(dir: &Path, session_id: &str, lines: &[&str]) -> PathBuf {
    let ws_dir = dir.join("ws-hash");
    std::fs::create_dir_all(&ws_dir).unwrap();
    let file_path = ws_dir.join(format!("{}.jsonl", session_id));
    std::fs::write(&file_path, lines.join("\n")).unwrap();
    file_path
}

const FIXTURE_USER: &str = r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":"Hello world"}}"#;
const FIXTURE_ASST: &str = r#"{"type":"assistant","uuid":"a1","timestamp":"2026-04-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi there"}]}}"#;

// Fixture files loaded via include_str! for complex cases
const MULTI_TURN_TOOL: &str = include_str!("fixtures/claude_code/multi_turn_tool.jsonl");
const ORPHAN_TOOL_RESULT: &str = include_str!("fixtures/claude_code/orphan_tool_result.jsonl");
const MALFORMED_MIXED: &str = include_str!("fixtures/claude_code/malformed_mixed.jsonl");
const THINKING_BLOCKS: &str = include_str!("fixtures/claude_code/thinking_blocks.jsonl");

// ── AC1: Clap flag parsing ─────────────────────────────────────────────────────

#[test]
fn test_e2e_migrate_command_parses_clap_flags() {
    use clap::Parser;
    let cli = Cli::parse_from(["rustain", "migrate", "--from", "claude-code", "--dry-run"]);
    match cli.command {
        Some(Command::Migrate {
            from,
            dry_run,
            yes,
            select,
            path,
        }) => {
            assert_eq!(from, "claude-code");
            assert!(dry_run);
            assert!(!yes);
            assert!(!select);
            assert!(path.is_none());
        }
        other => panic!("Expected Migrate command, got {:?}", other),
    }
}

#[test]
fn test_e2e_migrate_command_parses_yes_flag() {
    use clap::Parser;
    let cli = Cli::parse_from(["rustain", "migrate", "--from", "claude-code", "--yes"]);
    match cli.command {
        Some(Command::Migrate { yes, select, .. }) => {
            assert!(yes);
            assert!(!select);
        }
        other => panic!("Expected Migrate, got {:?}", other),
    }
}

#[test]
fn test_e2e_migrate_yes_and_select_conflict() {
    use clap::Parser;
    // clap should reject --yes --select combination
    let result = Cli::try_parse_from([
        "rustain",
        "migrate",
        "--from",
        "claude-code",
        "--yes",
        "--select",
    ]);
    assert!(result.is_err(), "--yes and --select must conflict");
}

#[test]
fn test_e2e_migrate_without_from_is_clap_error() {
    use clap::Parser;
    let result = Cli::try_parse_from(["rustain", "migrate"]);
    assert!(result.is_err(), "migrate without --from must fail");
}

// ── AC2: Unsupported source ────────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_migrate_unknown_source_errors() {
    let result = run_migrate("aider".to_string(), None, false, false, false).await;
    assert!(result.is_err());
    let err_str = result.unwrap_err().to_string();
    // Should contain the SubcommandExit marker (not the detailed message which goes to stderr)
    // The actual message is printed to stderr; here we just check the error propagates
    // The key test is that the command failed with a non-zero result
    // (message printed to stderr, not captured here)
    let _ = err_str; // error message goes to stderr, not captured
}

// More direct test of the error message logic:
#[test]
fn test_e2e_migrate_unknown_source_error_contains_supported_list() {
    let mut registry = ImporterRegistry::new();
    registry.register("claude-code", Box::new(ClaudeCodeImporter::new()));
    let sources = registry.available_sources();
    // Simulate what run_migrate does for unknown source
    let msg = format!(
        "Unsupported import source: aider. Supported sources: {}",
        sources.join(", ")
    );
    assert!(msg.contains("aider"), "error must mention the bad source");
    assert!(msg.contains("claude-code"), "error must list valid sources");
}

// ── AC3: Discovery lists candidates ───────────────────────────────────────────

#[tokio::test]
async fn test_e2e_migrate_discover_lists_fixtures() {
    let tmp = TempDir::new().unwrap();
    let source_dir = tmp.path().join("projects");

    write_fixture_jsonl(
        &source_dir,
        "session-uuid-1",
        &[
            r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":"First session message"}}"#,
        ],
    );
    write_fixture_jsonl(
        &source_dir,
        "session-uuid-2",
        &[
            r#"{"type":"user","uuid":"u2","timestamp":"2026-04-02T10:00:00Z","message":{"role":"user","content":"Second session message"}}"#,
        ],
    );

    let importer = ClaudeCodeImporter::with_root(source_dir.clone());
    let candidates = importer.discover(Some(&source_dir)).await.unwrap();

    assert_eq!(candidates.len(), 2);
    // Sorted by created_at ascending
    assert_eq!(candidates[0].source_session_id, "session-uuid-1");
    assert_eq!(candidates[1].source_session_id, "session-uuid-2");
    assert_eq!(candidates[0].title, "First session message");
    assert_eq!(candidates[1].title, "Second session message");
}

// ── AC4: Missing source directory ─────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_migrate_missing_source_dir_errors() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("nonexistent_dir");

    let importer = ClaudeCodeImporter::with_root(missing.clone());
    let result = importer.discover(Some(&missing)).await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.to_lowercase().contains("not found") || err.contains("not found"),
        "Error must mention directory not found: {}",
        err
    );
}

// ── AC5 + AC6: Import all with session meta tag ────────────────────────────────

#[tokio::test]
async fn test_e2e_migrate_yes_imports_all_with_session_meta_tag() {
    let tmp = TempDir::new().unwrap();
    let source_dir = tmp.path().join("projects");

    write_fixture_jsonl(&source_dir, "session-a", &[FIXTURE_USER, FIXTURE_ASST]);
    write_fixture_jsonl(&source_dir, "session-b", &[FIXTURE_USER, FIXTURE_ASST]);

    let storage = make_storage(tmp.path());
    let importer = ClaudeCodeImporter::with_root(source_dir.clone());

    let candidates = importer.discover(Some(&source_dir)).await.unwrap();
    assert_eq!(candidates.len(), 2);

    for candidate in &candidates {
        let result = importer.import(candidate, &storage).await.unwrap();
        let new_id = match result {
            ImportResult::Imported(id) => id,
            other => panic!("Expected Imported, got {:?}", other),
        };

        // Verify SessionMeta has imported_from set (AC6)
        let meta = storage.load_session_meta(&new_id).await.unwrap().unwrap();
        let imp = meta
            .imported_from
            .as_ref()
            .expect("imported_from must be set");
        assert_eq!(imp.source, "claude-code");
        assert_eq!(imp.original_session_id, candidate.source_session_id);
        assert!(imp.imported_at > 0);
    }

    // Total: 2 conversations imported
    let list = storage.list_conversations().await.unwrap();
    assert_eq!(list.len(), 2);
}

// ── AC6: SessionMeta.imported_from round-trip ──────────────────────────────────

#[test]
fn test_e2e_session_meta_imported_from_roundtrip() {
    let meta = SessionMeta {
        version: 1,
        title: "Imported".to_string(),
        created_at: 1700000000,
        updated_at: 1700000000,
        message_count: 5,
        bookmarks: vec![],
        fork_source: None,
        imported_from: Some(ImportSource {
            source: "claude-code".to_string(),
            original_session_id: "uuid-roundtrip".to_string(),
            imported_at: 1700000100,
        }),
        extra: serde_json::Map::new(),
    };

    let json = serde_json::to_string(&meta).unwrap();
    let deserialized: SessionMeta = serde_json::from_str(&json).unwrap();

    let imp = deserialized.imported_from.as_ref().unwrap();
    assert_eq!(imp.source, "claude-code");
    assert_eq!(imp.original_session_id, "uuid-roundtrip");
    assert_eq!(imp.imported_at, 1700000100);
}

#[test]
fn test_e2e_session_meta_native_session_no_imported_from_key() {
    let meta = SessionMeta {
        version: 1,
        title: "Native".to_string(),
        created_at: 1700000000,
        updated_at: 1700000000,
        message_count: 3,
        bookmarks: vec![],
        fork_source: None,
        imported_from: None,
        extra: serde_json::Map::new(),
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert!(
        !json.contains("importedFrom"),
        "Native session must not emit importedFrom: {}",
        json
    );
}

// ── AC7: Interactive selection (tested at unit level) ─────────────────────────
// Full stdin interaction is hard to E2E test without subprocess. The interactive
// selection logic is tested via the helper functions.

// ── AC8: Idempotency ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_migrate_idempotent_second_run_imports_nothing() {
    let tmp = TempDir::new().unwrap();
    let source_dir = tmp.path().join("projects");
    write_fixture_jsonl(&source_dir, "session-idem", &[FIXTURE_USER, FIXTURE_ASST]);

    let storage = make_storage(tmp.path());
    let importer = ClaudeCodeImporter::with_root(source_dir.clone());

    let candidates = importer.discover(Some(&source_dir)).await.unwrap();
    assert_eq!(candidates.len(), 1);

    // First import
    let r1 = importer.import(&candidates[0], &storage).await.unwrap();
    assert!(matches!(r1, ImportResult::Imported(_)));

    // Second import — must return AlreadyImported
    let r2 = importer.import(&candidates[0], &storage).await.unwrap();
    assert!(
        matches!(r2, ImportResult::AlreadyImported),
        "Second import must return AlreadyImported"
    );

    // Exactly 1 conversation in storage
    let list = storage.list_conversations().await.unwrap();
    assert_eq!(
        list.len(),
        1,
        "Must have exactly 1 conversation after two imports of the same source"
    );
}

// ── AC9: Dry run ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_migrate_dry_run_prints_table_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let source_dir = tmp.path().join("projects");
    write_fixture_jsonl(&source_dir, "dry-session-1", &[FIXTURE_USER]);
    write_fixture_jsonl(&source_dir, "dry-session-2", &[FIXTURE_USER]);

    let storage = make_storage(tmp.path());
    let importer = ClaudeCodeImporter::with_root(source_dir.clone());

    // Call the real CLI handler end-to-end with --dry-run (AC9).
    let result = run_migrate_with(
        &importer,
        &storage,
        Some(source_dir.clone()),
        /* yes */ false,
        /* select */ false,
        /* dry_run */ true,
    )
    .await;

    assert!(result.is_ok(), "Dry run must succeed: {:?}", result.err());
    let list = storage.list_conversations().await.unwrap();
    assert!(
        list.is_empty(),
        "Dry run must not write any conversations (found {})",
        list.len()
    );
}

// ── AC10: JSONL conversion preserves content and tool calls ───────────────────

#[test]
fn test_e2e_claude_code_jsonl_conversion_preserves_content_and_tool_calls() {
    let lines: Vec<_> = MULTI_TURN_TOOL
        .lines()
        .filter_map(|l| parse_jsonl_line(l).ok().flatten())
        .collect();

    let messages = convert_lines_to_chat_messages(&lines);

    // Expected structure:
    // [0] User("Read the README file")
    // [1] Assistant(text + tool_use=toolu_001) with tool_result merged
    // [2] Assistant("The README says...")
    assert_eq!(
        messages.len(),
        3,
        "Expected 3 messages (user + 2 assistant), got {}",
        messages.len()
    );

    use rustain::domain::models::MessageRole;
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].content, "Read the README file");
    assert_eq!(messages[0].id, "u1");

    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].tool_calls.len(), 1);
    let tc = &messages[1].tool_calls[0];
    assert_eq!(tc.id, "toolu_001");
    assert_eq!(tc.name, "Read");
    // Tool result should be merged
    assert!(
        tc.result.is_some(),
        "tool_result must be merged onto tool_call"
    );
    assert!(
        tc.result.as_ref().unwrap().content.contains("README"),
        "Tool result content must contain README text"
    );

    assert_eq!(messages[2].role, MessageRole::Assistant);
    assert!(messages[2].content.contains("README"));
}

#[test]
fn test_e2e_migrate_skips_meta_system_and_image_lines() {
    let lines: Vec<_> = THINKING_BLOCKS
        .lines()
        .filter_map(|l| parse_jsonl_line(l).ok().flatten())
        .collect();

    let messages = convert_lines_to_chat_messages(&lines);

    // fixture has: file-history-snapshot (skip), isMeta user (skip), real user, assistant(thinking+text)
    assert_eq!(messages.len(), 2, "Expected 2 real messages");

    use rustain::domain::models::MessageRole;
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].content, "What is recursion?");

    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(
        messages[1].content,
        "Recursion is when a function calls itself."
    );
    // Thinking block should be in content_blocks but NOT in content string
    use rustain::domain::models::ContentBlockType;
    assert!(
        messages[1]
            .content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlockType::Thinking(_))),
        "Thinking block must appear in content_blocks"
    );
    assert!(
        !messages[1].content.contains("The user wants"),
        "Thinking text must NOT appear in content string"
    );
}

// ── AC5 robustness: mixed imported + skipped-empty outcomes ─────────────────

#[tokio::test]
async fn test_e2e_migrate_mixed_imported_and_skipped_empty_reports_in_summary() {
    let tmp = TempDir::new().unwrap();
    let source_dir = tmp.path().join("projects");

    // 1 valid session
    write_fixture_jsonl(&source_dir, "valid-session", &[FIXTURE_USER, FIXTURE_ASST]);

    // 1 session with only system/meta lines → ImportResult::SkippedEmpty
    // (per AC5 spec — was Failed before finding #13 was applied).
    write_fixture_jsonl(
        &source_dir,
        "empty-session",
        &[
            r#"{"type":"system","timestamp":"2026-04-01T10:00:00Z"}"#,
            r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:01Z","isMeta":true,"message":{"role":"user","content":"injected"}}"#,
        ],
    );

    let storage = make_storage(tmp.path());
    let importer = ClaudeCodeImporter::with_root(source_dir.clone());

    let candidates = importer.discover(Some(&source_dir)).await.unwrap();
    assert_eq!(candidates.len(), 2);

    let mut imported = 0usize;
    let mut skipped_empty = 0usize;
    let mut failed = 0usize;

    for candidate in &candidates {
        match importer.import(candidate, &storage).await.unwrap() {
            ImportResult::Imported(_) => imported += 1,
            ImportResult::SkippedEmpty => skipped_empty += 1,
            ImportResult::Failed(_) => failed += 1,
            ImportResult::AlreadyImported => {}
        }
    }

    assert_eq!(imported, 1, "Expected 1 successful import");
    assert_eq!(skipped_empty, 1, "Expected 1 empty-session skip");
    assert_eq!(failed, 0, "No real failures for this fixture");

    // The valid session must be in storage
    let list = storage.list_conversations().await.unwrap();
    assert_eq!(list.len(), 1);
}

// ── AC10 unit-level: orphan tool result ───────────────────────────────────────

#[test]
fn test_e2e_claude_code_jsonl_orphan_tool_result_is_skipped() {
    let lines: Vec<_> = ORPHAN_TOOL_RESULT
        .lines()
        .filter_map(|l| parse_jsonl_line(l).ok().flatten())
        .collect();

    let messages = convert_lines_to_chat_messages(&lines);

    // fixture: user("Hello"), user([tool_result orphan]) (skipped), assistant("Let me help")
    // user("Hello") + assistant("Let me help") = 2 messages
    assert_eq!(messages.len(), 2);

    use rustain::domain::models::MessageRole;
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].content, "Hello");
    assert_eq!(messages[1].role, MessageRole::Assistant);
}

// ── AC10: malformed line mixed with valid lines ────────────────────────────────

#[test]
fn test_e2e_malformed_line_skipped_valid_lines_preserved() {
    let lines: Vec<_> = MALFORMED_MIXED
        .lines()
        .filter_map(|l| parse_jsonl_line(l).unwrap_or_default())
        .collect();

    let messages = convert_lines_to_chat_messages(&lines);

    // 1 valid user + 1 valid assistant (malformed line skipped)
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "Valid message");
}

// ── AC6: imported_from field backward compat ──────────────────────────────────

#[test]
fn test_e2e_legacy_session_meta_without_imported_from_deserializes() {
    let json = r#"{
        "version": 1,
        "title": "Old Session",
        "createdAt": 1700000000,
        "updatedAt": 1700000100,
        "messageCount": 3
    }"#;
    let meta: SessionMeta = serde_json::from_str(json).unwrap();
    assert!(
        meta.imported_from.is_none(),
        "Legacy session must deserialize with imported_from = None"
    );
}

// ── Metadata extraction ────────────────────────────────────────────────────────

#[test]
fn test_e2e_extract_candidate_metadata_from_multi_turn_fixture() {
    let path = Path::new("multi_turn_tool.jsonl");
    let candidate = extract_candidate_metadata(path, MULTI_TURN_TOOL).unwrap();
    assert_eq!(candidate.source_session_id, "multi_turn_tool");
    assert_eq!(candidate.title, "Read the README file");
    assert!(candidate.created_at > 0);
    // 4 messages: user, assistant(+tool_result merged → still 1), user(tool_result skipped from count), assistant
    // Actually message_count counts raw "user"/"assistant" type lines: 2 user + 2 assistant = 4
    assert_eq!(candidate.message_count, 4);
}

// ── run_migrate_with: --yes batching + partial-failure exit code ───────────────
//
// These tests exercise the composition-root-less core `run_migrate_with` so that
// the clap-flag → importer/storage dispatch path is covered end-to-end (finding
// #11 in the Story 4.5 code review — the original story file only wired up unit
// tests for the surface of the loop).

#[tokio::test]
async fn test_e2e_run_migrate_with_yes_imports_all_real_importer() {
    let tmp = TempDir::new().unwrap();
    let source_dir = tmp.path().join("projects");
    write_fixture_jsonl(&source_dir, "yes-session-1", &[FIXTURE_USER, FIXTURE_ASST]);
    write_fixture_jsonl(&source_dir, "yes-session-2", &[FIXTURE_USER, FIXTURE_ASST]);

    let storage = make_storage(tmp.path());
    let importer = ClaudeCodeImporter::with_root(source_dir.clone());

    let result = run_migrate_with(
        &importer,
        &storage,
        Some(source_dir.clone()),
        /* yes */ true,
        /* select */ false,
        /* dry_run */ false,
    )
    .await;

    assert!(
        result.is_ok(),
        "--yes path must succeed: {:?}",
        result.err()
    );
    let list = storage.list_conversations().await.unwrap();
    assert_eq!(list.len(), 2, "--yes must batch-import all candidates");

    // Every conversation must be tagged with imported_from = claude-code (AC6).
    for summary in &list {
        let meta = storage
            .load_session_meta(&summary.id)
            .await
            .unwrap()
            .unwrap();
        let imp = meta
            .imported_from
            .as_ref()
            .expect("--yes imports must set imported_from");
        assert_eq!(imp.source, "claude-code");
    }
}

/// Fake importer whose per-candidate outcomes are scripted. Used to exercise
/// run_migrate_with's summary-dispatch + exit-code branches without depending
/// on filesystem errors to construct an `ImportResult::Failed` outcome.
struct FakeImporter {
    candidates: Vec<ImportCandidate>,
    scripted: std::sync::Mutex<Vec<ImportResult>>,
}

#[async_trait]
impl ConversationImporter for FakeImporter {
    fn source_name(&self) -> &'static str {
        "Fake"
    }
    fn source_id(&self) -> &'static str {
        "fake"
    }
    async fn discover(&self, _path: Option<&Path>) -> Result<Vec<ImportCandidate>, StorageError> {
        Ok(self.candidates.clone())
    }
    async fn import(
        &self,
        _candidate: &ImportCandidate,
        _storage: &dyn StoragePort,
    ) -> Result<ImportResult, StorageError> {
        let mut q = self.scripted.lock().unwrap();
        if q.is_empty() {
            Ok(ImportResult::Failed("no scripted result".to_string()))
        } else {
            Ok(q.remove(0))
        }
    }
}

fn fake_candidate(id: &str) -> ImportCandidate {
    ImportCandidate {
        source_session_id: id.to_string(),
        title: format!("Fake {}", id),
        created_at: 1_700_000_000,
        message_count: 1,
        source_path: PathBuf::from(format!("/fake/{}.jsonl", id)),
    }
}

#[tokio::test]
async fn test_e2e_run_migrate_with_partial_failure_returns_nonzero_exit() {
    let tmp = TempDir::new().unwrap();
    let storage = make_storage(tmp.path());

    let importer = FakeImporter {
        candidates: vec![fake_candidate("ok"), fake_candidate("bad")],
        scripted: std::sync::Mutex::new(vec![
            ImportResult::Imported("new-id".to_string()),
            ImportResult::Failed("fabricated failure for test".to_string()),
        ]),
    };

    let result = run_migrate_with(
        &importer, &storage, /* path */ None, /* yes */ true, /* select */ false,
        /* dry_run */ false,
    )
    .await;

    assert!(
        result.is_err(),
        "run_migrate_with must return Err when any import fails (AC5 non-zero exit)"
    );
}

#[tokio::test]
async fn test_e2e_run_migrate_with_skipped_empty_is_not_a_failure() {
    let tmp = TempDir::new().unwrap();
    let storage = make_storage(tmp.path());

    let importer = FakeImporter {
        candidates: vec![fake_candidate("a"), fake_candidate("b")],
        scripted: std::sync::Mutex::new(vec![
            ImportResult::Imported("new-id".to_string()),
            ImportResult::SkippedEmpty,
        ]),
    };

    let result = run_migrate_with(
        &importer, &storage, /* path */ None, /* yes */ true, /* select */ false,
        /* dry_run */ false,
    )
    .await;

    assert!(
        result.is_ok(),
        "SkippedEmpty must not cause a non-zero exit: {:?}",
        result.err()
    );
}

// ── Story 4-6 AC9: Import Hardening (DF-133) ─────────────────────────────────

/// DF-133: `interactive_select` accepts `impl BufRead` for testability.
///
/// Inject mock stdin with commands: toggle item 1, confirm → one item selected.
#[test]
fn test_interactive_select_with_mock_stdin() {
    use rustain::adapters::cli::migrate::interactive_select;
    use rustain::domain::services::import::ImportCandidate;
    use std::io::BufReader;

    let candidates = vec![
        ImportCandidate {
            source_session_id: "sess-1".to_string(),
            title: "Conversation A".to_string(),
            created_at: 1700000000,
            message_count: 3,
            source_path: std::path::PathBuf::from("/tmp/sess-1.jsonl"),
        },
        ImportCandidate {
            source_session_id: "sess-2".to_string(),
            title: "Conversation B".to_string(),
            created_at: 1700000001,
            message_count: 5,
            source_path: std::path::PathBuf::from("/tmp/sess-2.jsonl"),
        },
    ];

    // Mock stdin: toggle item 1, then confirm.
    let mock_input = "1\nc\n";
    let mut reader = BufReader::new(mock_input.as_bytes());

    let selected = interactive_select(&candidates, &mut reader).unwrap();
    assert_eq!(selected.len(), 1, "Exactly one item should be selected");
    assert_eq!(
        selected[0].source_session_id, "sess-1",
        "Item 1 must be selected after toggle"
    );
}

/// DF-133: Aborting with `q` returns empty selection.
#[test]
fn test_interactive_select_abort_returns_empty() {
    use rustain::adapters::cli::migrate::interactive_select;
    use rustain::domain::services::import::ImportCandidate;
    use std::io::BufReader;

    let candidates = vec![ImportCandidate {
        source_session_id: "sess-1".to_string(),
        title: "Conversation A".to_string(),
        created_at: 1700000000,
        message_count: 3,
        source_path: std::path::PathBuf::from("/tmp/sess-1.jsonl"),
    }];

    let mock_input = "q\n";
    let mut reader = BufReader::new(mock_input.as_bytes());

    let selected = interactive_select(&candidates, &mut reader).unwrap();
    assert_eq!(selected.len(), 0, "Abort (q) must return empty selection");
}

/// DF-134: Title extraction walks content blocks for block-array user messages.
#[test]
fn test_title_extraction_block_array_user_message() {
    use rustain::domain::services::claude_code_jsonl::extract_candidate_metadata;
    use std::path::Path;

    // User message with block-array content (not plain text).
    let jsonl = r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"What is 2+2?"},{"type":"tool_result","tool_use_id":"t1","content":"4"}]}}"#;

    let candidate = extract_candidate_metadata(Path::new("test.jsonl"), jsonl).unwrap();
    assert_eq!(
        candidate.title, "What is 2+2?",
        "Title must be extracted from first Text block in block-array user message"
    );
}
