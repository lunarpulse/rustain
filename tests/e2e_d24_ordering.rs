//! D-24 A: session persistence ordering through the production TUI handlers and `run_turn`.
//!
//! K8 (plan summary), K9 (unsaved durable rewind tail), K10 (fork/rewind-fork and the first-tool
//! checkpoint) against a real `FileSystemStorage`. Interleavings are driven by one-shot
//! handshakes in a decorator that parks *inside* the real port call, never by sleeps, and the
//! decorator forwards every fenced entry point to the real store (it is not more permissive).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::e2e_fork_tail_after_plan::{
    conversation, durable_ids, ids, plan_run_tail, row, store, stored,
};
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::adapters::tui::handlers::transcript_target::{
    RewindFailure, handle_fork_confirm, handle_rewind_confirm, handle_rewind_fork_instead,
};
use rustain::domain::errors::StorageError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::router::{EscalationReason, ModelTier};
use rustain::domain::models::{
    CheckpointId, CompletionOptions, ContentBlockType, Conversation, ConversationSummary, Message,
    MessageRole, StopReason, StreamChunk, turn::migrate_chat_message_to_turn,
};
use rustain::domain::ports::{
    RewindStep, Rewound, SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
};
use rustain::domain::services::model_router::ResolvedModel;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::turn::run_turn;

fn sessions(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("sessions")
}

fn files_under(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.extension().is_none_or(|e| e != "seq") {
                out.push(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

// ── K8: the plan summary through the next committed turn ─────────────────────

/// Drain one committed assistant turn into the live conversation exactly as the event loop
/// does (`turns.push(committed)` then `rebuild_messages_mirror`).
fn commit_next_turn(conv: &mut Conversation, text: &str) {
    conv.messages.push(row(MessageRole::User, "next question"));
    let answer = row(MessageRole::Assistant, text);
    conv.turns.push(migrate_chat_message_to_turn(&answer));
    // The mirror materializes the turn's row; it must not drop the turn-less summary.
    conv.turns.last_mut().unwrap().id.0 = answer.id.clone();
    conv.rebuild_messages_mirror();
}

#[tokio::test]
async fn k8_the_plan_summary_is_one_durable_row_through_the_next_turn_and_reload() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let mut live = plan_run_tail(&store).await;
    let summary = live.messages.last().unwrap().clone();
    assert!(
        summary
            .content_blocks
            .contains(&ContentBlockType::PlanSummary)
    );

    commit_next_turn(&mut live, "the next answer");

    let in_memory: Vec<_> = live
        .messages
        .iter()
        .filter(|m| m.id == summary.id)
        .collect();
    assert_eq!(
        in_memory.len(),
        1,
        "the live mirror keeps the summary exactly once"
    );
    assert!(
        in_memory[0]
            .content_blocks
            .contains(&ContentBlockType::PlanSummary)
    );
    store
        .save_conversation(&live)
        .await
        .expect("the turn's save");

    let reloaded = stored(&store, "plan-run").await;
    let durable: Vec<_> = reloaded
        .messages
        .iter()
        .filter(|m| m.id == summary.id)
        .collect();
    assert_eq!(durable.len(), 1, "exactly once after reload");
    assert_eq!(durable[0].content, summary.content, "the same summary row");
    assert!(
        durable[0]
            .content_blocks
            .contains(&ContentBlockType::PlanSummary)
    );
    assert_eq!(
        durable_ids(&reloaded),
        durable_ids(&live),
        "reload equals the live durable history"
    );
    assert!(
        reloaded.messages.iter().all(|m| !m.is_ephemeral_display()),
        "the plan card stays ephemeral"
    );
    let turn = reloaded
        .turns
        .iter()
        .find(|t| t.id.0 == summary.id)
        .expect("the summary is turn-backed");
    assert!(
        turn.parts
            .iter()
            .all(|part| matches!(part, rustain::domain::models::turn::TurnPart::Prose { .. })),
        "no tool or provider replay was introduced for the summary"
    );
}

// ── decorator: parks inside the real port call ───────────────────────────────

#[derive(Default)]
struct Gate {
    reached: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Gate {
    fn arm() -> (Arc<Self>, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let gate = Arc::new(Self {
            reached: Mutex::new(Some(reached_tx)),
            release: Mutex::new(Some(release_rx)),
        });
        (gate, reached_rx, release_tx)
    }

    async fn park(&self) {
        let reached = self.reached.lock().unwrap().take();
        let release = self.release.lock().unwrap().take();
        if let (Some(reached), Some(release)) = (reached, release) {
            let _ = reached.send(());
            let _ = release.await;
        }
    }
}

/// Where the decorator parks: after the inner save was accepted (the flush is the accepted
/// predecessor) or before it is delivered (a captured pre-save in flight).
#[derive(Clone, Copy, PartialEq)]
enum Park {
    AfterSave,
    BeforeSave,
}

struct Parked {
    inner: Arc<FileSystemStorage>,
    gate: Arc<Gate>,
    at: Park,
}

#[async_trait]
impl StoragePort for Parked {
    async fn save_conversation(&self, conv: &Conversation) -> Result<(), StorageError> {
        if self.at == Park::BeforeSave {
            self.gate.park().await;
        }
        let result = self.inner.save_conversation(conv).await;
        if self.at == Park::AfterSave {
            self.gate.park().await;
        }
        result
    }
    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError> {
        self.inner.load_conversation(id).await
    }
    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>, StorageError> {
        self.inner.list_conversations().await
    }
    async fn create_checkpoint_for(
        &self,
        conv: &Conversation,
    ) -> Result<CheckpointId, StorageError> {
        self.inner.create_checkpoint_for(conv).await
    }
    async fn fork_conversation(
        &self,
        source: &Conversation,
        index: usize,
    ) -> Result<String, StorageError> {
        self.inner.fork_conversation(source, index).await
    }
    async fn rewind_conversation(
        &self,
        conv: &Conversation,
        index: usize,
    ) -> Result<Rewound, RewindStep> {
        self.inner.rewind_conversation(conv, index).await
    }
    async fn read_snapshot(
        &self,
        id: &str,
        checkpoint: CheckpointId,
        path: &Path,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_snapshot(id, checkpoint, path).await
    }
}

fn parked(dir: &TempDir, at: Park) -> (Parked, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (gate, reached, release) = Gate::arm();
    (
        Parked {
            inner: Arc::new(store(dir)),
            gate,
            at,
        },
        reached,
        release,
    )
}

fn base(rows: &[(&str, MessageRole, &str)]) -> Conversation {
    let mut conv = conversation("s1");
    for (id, role, text) in rows {
        let mut message = row(*role, text);
        message.id = (*id).to_string();
        conv.messages.push(message);
    }
    conv
}

// ── K9: rewind at an unsaved durable tail ────────────────────────────────────

#[tokio::test]
async fn k9_rewind_to_an_unsaved_durable_tail_keeps_the_chosen_row_and_it_survives_reload() {
    // The plan summary is the one durable row not saved yet; rewinding AT it keeps it.
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let live = plan_run_tail(&store).await;
    let summary = live.messages.last().unwrap().id.clone();
    let target = live.messages.len() - 1;
    let rewound = handle_rewind_confirm(&store, &mut None, &live, target)
        .await
        .expect("rewind");
    assert_eq!(
        durable_ids(&rewound.conversation),
        durable_ids(&live),
        "no durable row of the chosen history was lost"
    );
    assert_eq!(rewound.conversation.messages.last().unwrap().id, summary);
    assert_eq!(
        ids(&stored(&store, "plan-run").await),
        durable_ids(&live),
        "reload agrees"
    );
    assert!(
        !sessions(&dir)
            .join("plan-run")
            .join("rewind_txn.json")
            .exists(),
        "the journal is gone"
    );

    // Independence: another real durable row, unsaved, is kept the same way.
    let mut live = rewound.conversation;
    live.messages
        .push(row(MessageRole::User, "an unsaved question"));
    let unsaved = live.messages.last().unwrap().id.clone();
    let rewound = handle_rewind_confirm(&store, &mut None, &live, live.messages.len() - 1)
        .await
        .expect("rewind at the unsaved question");
    assert_eq!(rewound.conversation.messages.last().unwrap().id, unsaved);
    assert!(ids(&stored(&store, "plan-run").await).contains(&unsaved));

    // Positive control: a saved baseline still truncates exactly, with the prefix intact.
    let cut = handle_rewind_confirm(&store, &mut None, &rewound.conversation, 1)
        .await
        .expect("saved rewind");
    assert_eq!(ids(&cut.conversation), ids(&live)[..2].to_vec());
    assert_eq!(
        ids(&stored(&store, "plan-run").await),
        ids(&live)[..2].to_vec()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k9_a_writer_accepted_after_the_flush_refuses_the_rewind_before_any_effect() {
    let dir = TempDir::new().unwrap();
    let workspace_file = dir.path().join("tool_made.txt");
    std::fs::write(&workspace_file, b"untouched").unwrap();
    let mut live = base(&[
        ("u1", MessageRole::User, "one"),
        ("a1", MessageRole::Assistant, "two"),
        ("u2", MessageRole::User, "three"),
    ]);
    store(&dir).save_conversation(&live).await.unwrap();
    live.messages[2].content = "three, flushed by the rewind".into();
    let (parked, reached, release) = parked(&dir, Park::AfterSave);
    let rewinder = {
        let live = live.clone();
        tokio::spawn(async move { handle_rewind_confirm(&parked, &mut None, &live, 1).await })
    };
    reached.await.unwrap(); // the flush was accepted; the rewind has not begun
    // A competing writer is accepted at the same index with a different payload.
    let competitor = store(&dir);
    let mut b = competitor.load_conversation("s1").await.unwrap().unwrap();
    b.messages[1].content = "two, from the competitor".into();
    competitor.save_conversation(&b).await.unwrap();
    release.send(()).unwrap();
    let outcome = rewinder.await.unwrap();
    assert!(
        matches!(outcome, Err(RewindFailure::Begin(StorageError::Stale(_)))),
        "refused before the first journal or workspace effect: {:?}",
        outcome.err()
    );
    assert!(
        !sessions(&dir).join("s1").join("rewind_txn.json").exists(),
        "no journal was started"
    );
    assert_eq!(std::fs::read(&workspace_file).unwrap(), b"untouched");
    let disk = stored(&competitor, "s1").await;
    assert_eq!(
        disk.messages[1].content, "two, from the competitor",
        "the winner's row is exactly as committed"
    );
    assert_eq!(disk.messages.len(), 3, "nothing was truncated or installed");
}

// ── K10: fork and rewind-fork-instead ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k10_both_fork_confirmations_project_the_accepted_flush_or_refuse_before_any_new_id_write()
{
    for entry in 0..2 {
        let dir = TempDir::new().unwrap();
        let mut live = base(&[
            ("u1", MessageRole::User, "one"),
            ("a1", MessageRole::Assistant, "two"),
            ("u2", MessageRole::User, "three"),
        ]);
        store(&dir).save_conversation(&live).await.unwrap();
        live.messages[1].content = "two, flushed by the fork".into();
        let (parked, reached, release) = parked(&dir, Park::AfterSave);
        let forker = {
            let live = live.clone();
            tokio::spawn(async move {
                if entry == 0 {
                    handle_fork_confirm(&parked, &mut None, false, &live, 1).await
                } else {
                    handle_rewind_fork_instead(&parked, &mut None, false, &live, 1).await
                }
            })
        };
        reached.await.unwrap();
        let competitor = store(&dir);
        let mut b = competitor.load_conversation("s1").await.unwrap().unwrap();
        b.messages[1].content = "two, from the competitor".into();
        competitor.save_conversation(&b).await.unwrap();
        let before = files_under(&sessions(&dir));
        release.send(()).unwrap();
        let outcome = forker.await.unwrap();
        assert!(
            matches!(outcome, Err(StorageError::Stale(_))),
            "entry {entry}: {outcome:?}"
        );
        assert_eq!(
            files_under(&sessions(&dir)),
            before,
            "entry {entry}: no new-id write"
        );
        assert_eq!(competitor.list_conversations().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn k10_without_a_competitor_the_fork_holds_exactly_the_flushed_prefix() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let mut live = base(&[
        ("u1", MessageRole::User, "one"),
        ("a1", MessageRole::Assistant, "two"),
        ("u2", MessageRole::User, "three"),
    ]);
    store.save_conversation(&live).await.unwrap();
    live.messages[1].content = "two, flushed by the fork".into();
    let new_id = handle_fork_confirm(&store, &mut None, false, &live, 1)
        .await
        .expect("fork");
    let forked = stored(&store, &new_id).await;
    assert_eq!(ids(&forked), ["u1", "a1"]);
    assert_eq!(forked.messages[1].content, "two, flushed by the fork");
    assert_eq!(forked.fork_source.as_ref().unwrap().conversation_id, "s1");
}

// ── K10: the first tool's checkpoint through run_turn ────────────────────────

struct WriteOnce {
    calls: AtomicUsize,
    path: String,
}

#[async_trait]
impl StreamingProvider for WriteOnce {
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
                    input: serde_json::json!({"file_path": self.path, "content": "written by the tool\n"}),
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
        "mock".into()
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
        _path: &Path,
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

/// Run one turn whose first tool is a Write. `snapshot` is the conversation the turn was
/// started from (the clone captured at submit); `storage` is the port the producer saves to.
async fn drive_write_turn(
    dir: &TempDir,
    storage: Arc<dyn StoragePort>,
    snapshot: Conversation,
    target: &Path,
) -> Vec<StreamChunk> {
    let sessions_dir = sessions(dir);
    let tool_store = Arc::new(FileSystemStorage::with_workspace_root(
        sessions_dir,
        dir.path().to_path_buf(),
    ));
    let adapter = ToolSetAdapter::new(
        dir.path().to_path_buf(),
        tool_store,
        Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::SandboxPolicy::Permissive,
        )),
    );
    let provider: Arc<dyn StreamingProvider> = Arc::new(WriteOnce {
        calls: AtomicUsize::new(0),
        path: target.to_string_lossy().into_owned(),
    });
    let security: Arc<dyn SecurityPort> = Arc::new(AllowAll);
    let tools: Arc<dyn ToolSetPort> = Arc::new(adapter);
    let approval = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval, 16);
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
            provider_replay: vec![],
        }],
        CompletionOptions {
            session_id: None,
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
        snapshot.id.clone(),
        storage,
        snapshot,
        None,
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
        "sess-d24".into(),
        rustain::domain::models::TurnOrigin::Interactive,
        false,
        None,
        None,
    )
    .await;
    let mut chunks = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::ProviderChunk { chunk, .. } = event {
            chunks.push(chunk);
        }
    }
    chunks
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k10_a_superseded_pre_save_creates_no_checkpoint_and_dispatches_no_tool() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("made_by_tool.txt");
    let writer = Arc::new(store(&dir));
    writer
        .save_conversation(&base(&[("u1", MessageRole::User, "write the file")]))
        .await
        .unwrap();
    // The turn starts from this capture (cloned at submit) ...
    let snapshot = writer.load_conversation("s1").await.unwrap().unwrap();
    let submit_capture = snapshot.capture();
    // ... its pre-save is parked after capture, and a later flush is accepted first.
    let (parked, reached, release) = parked(&dir, Park::BeforeSave);
    let storage: Arc<dyn StoragePort> = Arc::new(parked);
    let run = drive_write_turn(&dir, storage, submit_capture, &target);
    tokio::pin!(run);
    tokio::select! {
        _ = &mut run => panic!("the turn finished before its pre-save was released"),
        _ = reached => {}
    }
    let newer = store(&dir);
    let mut flushed = newer.load_conversation("s1").await.unwrap().unwrap();
    flushed
        .messages
        .push(row(MessageRole::Assistant, "newer accepted flush"));
    newer.save_conversation(&flushed).await.unwrap();
    release.send(()).unwrap();
    let chunks = run.await;
    assert!(
        !target.exists(),
        "no checkpoint-dependent tool effect for stale authority"
    );
    assert!(
        !sessions(&dir).join("s1").join("checkpoints.json").exists(),
        "no checkpoint was created"
    );
    assert!(
        chunks.iter().any(
            |c| matches!(c, StreamChunk::Error { content } if content.contains("changed elsewhere"))
        ),
        "the refusal is surfaced: {chunks:?}"
    );
    assert_eq!(
        ids(&stored(&newer, "s1").await),
        ids(&flushed),
        "the late pre-save did not overwrite the flush"
    );
}

#[tokio::test]
async fn k10_a_fresh_accepted_base_creates_the_exact_checkpoint_and_runs_the_tool() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("made_by_tool.txt");
    let writer = Arc::new(store(&dir));
    writer
        .save_conversation(&base(&[("u1", MessageRole::User, "write the file")]))
        .await
        .unwrap();
    let snapshot = writer.load_conversation("s1").await.unwrap().unwrap();
    let chunks = drive_write_turn(&dir, writer.clone(), snapshot, &target).await;
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "written by the tool\n"
    );
    let log = writer.list_checkpoints("s1").await.unwrap();
    assert_eq!(
        log.iter().map(|c| c.id).collect::<Vec<_>>(),
        [CheckpointId(1)]
    );
    assert!(
        !chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::Error { .. })),
        "{chunks:?}"
    );
}
