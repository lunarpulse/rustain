//! Story 19.1 conformance — the line-diff channel for Write/Edit tool blocks.
//!
//! Source of truth: `_bmad-output/implementation-artifacts/19-1-line-diff-in-tool-blocks.md`
//! (Acceptance Criteria as amended by the 2026-08-27 code review: the channel
//! carries `WriteDiffState`, not `Option<Vec<DiffLine>>`).
//!
//! - **AC2 keystone (front door, AI-17.1 Rule 2):** `run_turn` itself drives a
//!   real `Write` over an existing file under an active checkpoint, and the
//!   assertion is made on the `StreamChunk::ToolResult` the turn EMITS. The
//!   previous version of this file called `write_display_diff` directly, so
//!   deleting `run_turn`'s invocation left it green — the same dead-caller
//!   false-green that put this story on the board. Mutant 1 now bites.
//! - **P2 key round-trip:** `snapshot_file` and `read_snapshot` must derive an
//!   identical key. Nothing enforced that before; it was a comment.
//! - **AC3 pins:** the provider-facing result literals and a compile-time
//!   three-field `ToolResult` literal.
//! - **AC4:** every "not captured" render names its real reason.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::events::AppEvent;
use rustain::domain::models::checkpoint::CheckpointId;
use rustain::domain::models::router::{EscalationReason, ModelTier};
use rustain::domain::models::{
    ChatMessage, CompletionOptions, Conversation, DiffKind, Message, MessageRole,
    NotCapturedReason, StopReason, StreamChunk, ToolCallRequest, ToolResult, WriteDiffState,
};
use rustain::domain::ports::{SecurityPort, StoragePort, StreamingProvider, ToolSetPort};
use rustain::domain::services::model_router::ResolvedModel;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::turn::{run_turn, write_display_diff};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const CONVERSATION: &str = "conv-19-1-keystone";

fn no_paths() -> std::collections::HashSet<std::path::PathBuf> {
    std::collections::HashSet::new()
}

fn make_adapter(dir: &std::path::Path) -> (Arc<ToolSetAdapter>, Arc<FileSystemStorage>) {
    let sessions_dir = dir.join(".claude").join("sessions");
    let storage = Arc::new(FileSystemStorage::with_workspace_root(
        sessions_dir,
        dir.to_path_buf(),
    ));
    let adapter = ToolSetAdapter::new(
        dir.to_path_buf(),
        storage.clone(),
        Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::SandboxPolicy::Permissive,
        )),
    );
    (Arc::new(adapter), storage)
}

// ---------------------------------------------------------------------------
// AC2 keystone — through the front door: run_turn, not the helper.
// ---------------------------------------------------------------------------

/// Provider that asks for one Write and then ends the turn.
struct WriteOnceProvider {
    calls: AtomicUsize,
    input: serde_json::Value,
}

#[async_trait]
impl StreamingProvider for WriteOnceProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = StreamChunk> + Send>>,
        rustain::domain::errors::ProviderError,
    > {
        let chunks = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                StreamChunk::ToolUse {
                    id: "tool-write-1".into(),
                    name: "Write".into(),
                    input: self.input.clone(),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            }]
        };
        Ok(Box::pin(futures::stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "mock".to_string()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }
}

struct AllowAll;

#[async_trait]
impl SecurityPort for AllowAll {
    fn check_blocklist(
        &self,
        _command: &str,
    ) -> Result<(), rustain::domain::errors::PermissionError> {
        Ok(())
    }
    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: rustain::domain::models::FileOperation,
    ) -> Result<rustain::domain::models::PathAccessType, rustain::domain::errors::PermissionError>
    {
        Ok(rustain::domain::models::PathAccessType::Workspace)
    }
    fn current_mode(&self) -> rustain::domain::models::PermissionMode {
        rustain::domain::models::PermissionMode::Yolo
    }
    fn set_mode(&self, _mode: rustain::domain::models::PermissionMode) {}
}

/// Storage whose snapshot read-back always fails. Needed to pin the reason
/// for a READ ERROR, which is a different state from "no snapshot exists" —
/// conflating the two is how the UI came to claim "no active checkpoint" for
/// a decode failure under a perfectly live checkpoint.
struct FailingSnapshotStorage;

#[async_trait]
impl StoragePort for FailingSnapshotStorage {
    async fn save_conversation(
        &self,
        _conv: &Conversation,
    ) -> Result<(), rustain::domain::errors::StorageError> {
        Ok(())
    }
    async fn load_conversation(
        &self,
        _id: &str,
    ) -> Result<Option<Conversation>, rustain::domain::errors::StorageError> {
        Ok(None)
    }
    async fn list_conversations(
        &self,
    ) -> Result<
        Vec<rustain::domain::models::ConversationSummary>,
        rustain::domain::errors::StorageError,
    > {
        Ok(vec![])
    }
    async fn read_snapshot(
        &self,
        _conversation_id: &str,
        _checkpoint: CheckpointId,
        _path: &std::path::Path,
    ) -> Result<Option<Vec<u8>>, rustain::domain::errors::StorageError> {
        Err(rustain::domain::errors::StorageError::IoError(
            "snapshot is corrupt".into(),
        ))
    }
}

fn empty_conversation() -> Conversation {
    Conversation {
        id: CONVERSATION.to_string(),
        title: "t".into(),
        messages: vec![ChatMessage {
            synthetic: false,
            id: CONVERSATION.to_string(),
            content_blocks: vec![],
            role: MessageRole::User,
            content: "write the file".into(),
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
            authorship: Default::default(),
            retracted_at_ms: None,
        }],
        turns: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    }
}

/// **AC2 keystone.** A real Write over an existing file, driven by `run_turn`,
/// asserted on the chunk the turn emits.
///
/// ⛔ Mutant 1 (delete the `write_display_diff` call in `run_turn`): this test
/// goes RED because the emitted chunk carries `NotAWrite` instead of a `Diff`.
/// The old direct-helper version of this test could not see that.
#[tokio::test]
async fn ac2_keystone_run_turn_emits_the_real_overwrite_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("existing.rs");
    std::fs::write(&file, "x\ny\n").unwrap();
    let (adapter, storage) = make_adapter(tmp.path());

    let input = serde_json::json!({
        "file_path": file.to_str().unwrap(),
        "content": "x\nz\n"
    });

    let provider: Arc<dyn StreamingProvider> = Arc::new(WriteOnceProvider {
        calls: AtomicUsize::new(0),
        input: input.clone(),
    });
    let security: Arc<dyn SecurityPort> = Arc::new(AllowAll);
    let tools: Arc<dyn ToolSetPort> = adapter.clone();
    let approval_runtime = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval_runtime, 16);

    let (tx, mut rx) = mpsc::unbounded_channel();
    run_turn(
        provider,
        vec![Message {
            role: MessageRole::User,
            content: "write the file".into(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
            reasoning_content: None,
        }],
        CompletionOptions {
            model: "test".into(),
            max_tokens: 100,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
        },
        tx,
        security,
        tools,
        scheduler,
        CONVERSATION.into(),
        storage.clone(),
        empty_conversation(),
        None,
        CancellationToken::new(),
        Arc::new(rustain::adapters::noop::NoOpUsageLedger),
        ResolvedModel {
            model: "test".into(),
            tier: ModelTier::CheapAgentic,
            escalation_reason: EscalationReason::None,
        },
        None,
        0,
        None,
        "sess-19-1".into(),
        rustain::domain::models::TurnOrigin::Interactive,
        false,
        None,
    )
    .await;

    let mut states = Vec::new();
    let mut contents = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AppEvent::ProviderChunk {
            chunk: StreamChunk::ToolResult { diff, content, .. },
            ..
        } = ev
        {
            states.push(diff);
            contents.push(content);
        }
    }
    assert_eq!(states.len(), 1, "one Write, one tool result: {states:?}");

    // AC3, pinned from the same production run: the model's bytes are the
    // pre-19.1 literal, with no diff spliced in.
    assert_eq!(
        contents[0],
        format!(
            "Successfully wrote {} bytes to {}",
            "x\nz\n".len(),
            file.display()
        ),
        "the model-facing bytes are unchanged (A5)"
    );

    match &states[0] {
        WriteDiffState::Diff { lines, .. } => {
            assert!(
                lines
                    .iter()
                    .any(|d| d.kind == DiffKind::Removed && d.content == "y"),
                "the removed line proves the snapshot read-back rather than an \
                 all-additions fabrication: {lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|d| d.kind == DiffKind::Added && d.content == "z"),
                "{lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|d| d.kind == DiffKind::Context && d.content == "x"),
                "{lines:?}"
            );
        }
        other => panic!("run_turn must emit a real diff for an overwrite, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P2 — the key-agreement invariant, now enforced instead of commented.
// ---------------------------------------------------------------------------

/// `snapshot_file` and `read_snapshot` must derive the same key for the same
/// file. Winston rated this an amber invariant that nothing enforced; these are
/// the four shapes that can make the two derivations disagree.
///
/// ⛔ Mutant 4 (look the path up in a different form than it was snapshotted):
/// the absolute/relative case goes RED.
#[tokio::test]
async fn p2_snapshot_and_read_back_agree_on_the_key() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tokio::fs::canonicalize(tmp.path()).await.unwrap();
    let sessions_dir = root.join(".claude").join("sessions");
    let storage = FileSystemStorage::with_workspace_root(sessions_dir, root.clone());
    let cp = CheckpointId(11);

    // (a) An existing file addressed absolutely.
    let existing = root.join("a.rs");
    tokio::fs::write(&existing, b"one\n").await.unwrap();
    storage
        .snapshot_file(CONVERSATION, cp, &existing, b"one\n")
        .await
        .unwrap();
    assert_eq!(
        storage
            .read_snapshot(CONVERSATION, cp, &existing)
            .await
            .unwrap()
            .as_deref(),
        Some(&b"one\n"[..]),
        "absolute path round-trip"
    );

    // (b) A file that did NOT exist when snapshotted — `snapshot_file` keys it
    // through the parent, and by read-back time the write has created it.
    let created = root.join("b.rs");
    storage
        .snapshot_file(CONVERSATION, cp, &created, b"")
        .await
        .unwrap();
    tokio::fs::write(&created, b"new\n").await.unwrap();
    assert_eq!(
        storage
            .read_snapshot(CONVERSATION, cp, &created)
            .await
            .unwrap()
            .as_deref(),
        Some(&b""[..]),
        "a new file's snapshot must still be findable after the write created it"
    );

    // (c) A symlink whose target did not exist at snapshot time. The write
    // follows the link, so a naive re-canonicalize hashes the target instead
    // and misses forever.
    #[cfg(unix)]
    {
        let target = root.join("target.rs");
        let link = root.join("link.rs");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        storage
            .snapshot_file(CONVERSATION, cp, &link, b"")
            .await
            .unwrap();
        tokio::fs::write(&target, b"through-the-link\n")
            .await
            .unwrap();
        assert_eq!(
            storage
                .read_snapshot(CONVERSATION, cp, &link)
                .await
                .unwrap()
                .as_deref(),
            Some(&b""[..]),
            "a dangling-symlink write must still find its snapshot"
        );
    }

    // (d) A path outside the workspace has no snapshot and must not panic.
    assert!(
        storage
            .read_snapshot(CONVERSATION, cp, std::path::Path::new("/etc/hosts"))
            .await
            .unwrap()
            .is_none()
    );
}

/// The resolution helper is what keeps the write and read paths in agreement
/// for relative model paths. Correctness used to depend on the coincidence
/// that the process CWD equalled the workspace root.
#[test]
fn p2_relative_paths_resolve_against_the_workspace_not_the_cwd() {
    use rustain::infrastructure::runtime::turn::resolve_write_path;
    let ws = std::path::Path::new("/workspace/project");
    assert_eq!(
        resolve_write_path(Some(ws), "src/main.rs"),
        Some(std::path::PathBuf::from("/workspace/project/src/main.rs"))
    );
    assert_eq!(
        resolve_write_path(Some(ws), "/absolute/elsewhere.rs"),
        Some(std::path::PathBuf::from("/absolute/elsewhere.rs"))
    );
    // No workspace root and a relative path: decline rather than guess.
    assert_eq!(resolve_write_path(None, "src/main.rs"), None);
}

// ---------------------------------------------------------------------------
// AC4 — every unavailable state names its own real reason.
// ---------------------------------------------------------------------------

/// ⛔ Mutant 2 (return `NewFile`/empty instead of `SnapshotUnavailable`): RED.
/// ⛔ Mutant 3 (launder a snapshot read ERROR as "no active checkpoint"): RED
/// here. A live checkpoint whose snapshot cannot be read or decoded is a real
/// production failure mode, and telling the user the checkpoint was missing is
/// an affirmative false statement about their session.
#[tokio::test]
async fn ac4_a_read_error_is_not_reported_as_a_missing_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("f.rs");
    std::fs::write(&file, "a\n").unwrap();
    let request = ToolCallRequest {
        id: "t1".into(),
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": file.to_str().unwrap(), "content": "b\n" }),
    };
    let state = write_display_diff(
        &FailingSnapshotStorage,
        CONVERSATION,
        CheckpointId(42),
        &request,
        Some(tmp.path()),
        &no_paths(),
    )
    .await;
    assert_eq!(
        state,
        WriteDiffState::NotCaptured {
            reason: NotCapturedReason::SnapshotReadFailed
        },
        "a read failure must name itself, not blame a missing checkpoint"
    );
}

#[tokio::test]
async fn ac4_site_names_the_real_reason_for_each_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("f.rs");
    std::fs::write(&file, "a\n").unwrap();
    let (_adapter, storage) = make_adapter(tmp.path());
    let request = ToolCallRequest {
        id: "t1".into(),
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": file.to_str().unwrap(), "content": "b\n" }),
    };

    // Sentinel checkpoint: run_turn fell through because creation failed, so
    // no snapshot can exist and the reason is exactly AC4's.
    assert_eq!(
        write_display_diff(
            storage.as_ref(),
            CONVERSATION,
            CheckpointId(0),
            &request,
            Some(tmp.path()),
            &no_paths(),
        )
        .await,
        WriteDiffState::NotCaptured {
            reason: NotCapturedReason::NoActiveCheckpoint
        }
    );

    // A live checkpoint with no snapshot behind it is a DIFFERENT reason, and
    // must not claim the checkpoint was missing.
    assert_eq!(
        write_display_diff(
            storage.as_ref(),
            CONVERSATION,
            CheckpointId(42),
            &request,
            Some(tmp.path()),
            &no_paths(),
        )
        .await,
        WriteDiffState::NotCaptured {
            reason: NotCapturedReason::SnapshotUnavailable
        }
    );

    // A non-Write is not a "failure" at all.
    let other = ToolCallRequest {
        id: "t2".into(),
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": "ls" }),
    };
    assert_eq!(
        write_display_diff(
            storage.as_ref(),
            CONVERSATION,
            CheckpointId(42),
            &other,
            Some(tmp.path()),
            &no_paths(),
        )
        .await,
        WriteDiffState::NotAWrite
    );
}

/// ⛔ Mutant 5 (let a second write in one batch read the first's snapshot):
/// RED here. Snapshots are first-write-wins per `(checkpoint, path)`, so the
/// second write cannot establish its own original and must say so rather than
/// present both writes' changes as its own.
#[tokio::test]
async fn ac4_second_write_to_one_path_in_a_batch_declines_instead_of_lying() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("twice.rs");
    std::fs::write(&file, "A\n").unwrap();
    let (_adapter, storage) = make_adapter(tmp.path());
    let request = ToolCallRequest {
        id: "t1".into(),
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": file.to_str().unwrap(), "content": "C\n" }),
    };

    let mut already = no_paths();
    already.insert(file.clone());

    assert_eq!(
        write_display_diff(
            storage.as_ref(),
            CONVERSATION,
            CheckpointId(9),
            &request,
            Some(tmp.path()),
            &already,
        )
        .await,
        WriteDiffState::NotCaptured {
            reason: NotCapturedReason::SupersededInBatch
        },
        "the second write must not diff against the pre-FIRST-write original"
    );
}

// ---------------------------------------------------------------------------
// AC3 — the provider-facing shape and literals.
// ---------------------------------------------------------------------------

/// (a) Edit's success string is the SAME literal (Edit delegates to
///     `execute_write`), pinned from a real execution.
/// (b) `ToolResult` names exactly its three fields at compile time: a fourth
///     (diff) field cannot be added without breaking this test binary.
#[tokio::test]
async fn ac3_pins_provider_facing_literals_and_toolresult_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("edit.rs");
    std::fs::write(&file, "a\nb\n").unwrap();
    let (adapter, _storage) = make_adapter(tmp.path());
    adapter
        .set_execution_context(CONVERSATION.to_string(), CheckpointId(3), 0)
        .await;

    let result = adapter
        .execute(
            "Edit",
            serde_json::json!({
                "file_path": file.to_str().unwrap(),
                "old_string": "b",
                "new_string": "c"
            }),
            CancellationToken::new(),
        )
        .await
        .expect("Edit executes");
    assert!(!result.is_error, "Edit should succeed: {}", result.content);
    assert_eq!(
        result.content,
        format!(
            "Successfully wrote {} bytes to {}",
            "a\nc\n".len(),
            file.display()
        ),
        "Edit shares the Write literal, and neither gained a diff (A5/AC3)"
    );

    // Compile-time three-field pin: adding a field to `ToolResult` breaks this.
    let pinned = ToolResult {
        tool_use_id: "id".to_string(),
        content: "c".to_string(),
        is_error: false,
    };
    assert_eq!(pinned.tool_use_id, "id");
}

// ---------------------------------------------------------------------------
// Persistence — the states must survive a round trip, and an absent field must
// never be read as "the file was new".
// ---------------------------------------------------------------------------

/// ⛔ Mutant 10 (deserialize an absent pre-19.1 field as `NewFile` or a real
/// diff): RED here. That fabrication is what painted an all-additions diff
/// over a replaced file for every upgraded session.
#[test]
fn legacy_records_deserialize_to_unknown_provenance() {
    #[derive(serde::Deserialize)]
    struct Legacy {
        #[serde(default)]
        diff: WriteDiffState,
    }
    let restored: Legacy = serde_json::from_str(r#"{"content":"x","isError":false}"#).unwrap();
    assert_eq!(
        restored.diff,
        WriteDiffState::NotCaptured {
            reason: NotCapturedReason::HistoricalOrReattached
        },
        "an absent field means unknown provenance, never a new file"
    );
}

/// Every state survives serialisation, so the journal and the persisted
/// conversation cannot silently collapse two of them into one.
#[test]
fn every_state_round_trips() {
    let states = vec![
        WriteDiffState::NotAWrite,
        WriteDiffState::NewFile,
        WriteDiffState::Diff {
            lines: vec![],
            more: 0,
        },
        WriteDiffState::from_original(b"a\n", "b\n"),
        WriteDiffState::NotCaptured {
            reason: NotCapturedReason::SnapshotReadFailed,
        },
    ];
    for s in states {
        let json = serde_json::to_string(&s).unwrap();
        let back: WriteDiffState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back, "round trip failed for {json}");
    }
}
