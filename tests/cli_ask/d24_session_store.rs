//! D-24 A: `ask` against a real `FileSystemStorage`.
//!
//! Every eval-5 `ask` test uses `NoOpStorage`, so none proves persistence. These run the real
//! `run_ask_core` over a TempDir store: `--dry-run` takes no session/checkpoint/snapshot/authority
//! write path (including when the model asks for a tool), and the resume hint is printed only for
//! a final save the store accepted.

use std::io::Cursor;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::stream;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
};
use rustain::domain::models::{
    ChatMessage, CompletionOptions, Conversation, Message, MessageRole, StopReason, StreamChunk,
};
use rustain::domain::ports::{StoragePort, StreamingProvider};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::composition::CliCore;
use tempfile::TempDir;

use super::{default_config, make_dry_run_opts, make_opts};

/// First call: optionally a tool use; every later call: a final text answer. Before the first
/// call returns, `competitor` (a second adapter on the same store) commits a newer row.
struct Scripted {
    calls: AtomicUsize,
    ask_for_tool: bool,
    competitor: Option<(Arc<FileSystemStorage>, String)>,
}

#[async_trait]
impl StreamingProvider for Scripted {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if first && let Some((store, id)) = &self.competitor {
            let mut conv = store.load_conversation(id).await.unwrap().unwrap();
            conv.messages.push(ChatMessage {
                id: "a-competitor".into(),
                role: MessageRole::Assistant,
                content: "a newer accepted row".into(),
                ..Default::default()
            });
            store.save_conversation(&conv).await.unwrap();
        }
        let chunks = if first && self.ask_for_tool {
            vec![
                StreamChunk::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "/nonexistent"}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![
                StreamChunk::Text {
                    content: "final answer".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        };
        Ok(Box::pin(stream::iter(chunks)))
    }
    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "mock-d24".into()
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

fn core_over(provider: Arc<dyn StreamingProvider>, storage: Arc<dyn StoragePort>) -> CliCore {
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let security: Arc<dyn rustain::domain::ports::SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn rustain::domain::ports::ToolSetPort> = Arc::new(NoOpToolSet);
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 16);
    CliCore {
        provider,
        security,
        tools,
        tool_scheduler,
        approval,
        storage,
        event_tx,
        event_rx,
        ledger: Arc::new(NoOpUsageLedger),
        persona: Arc::new(rustain::adapters::noop::NoOpPersona),
    }
}

fn store_in(dir: &TempDir) -> Arc<FileSystemStorage> {
    Arc::new(FileSystemStorage::with_workspace_root(
        dir.path().join(".claude").join("sessions"),
        dir.path().to_path_buf(),
    ))
}

async fn seeded(store: &FileSystemStorage, id: &str) {
    let mut conv = Conversation {
        id: id.into(),
        title: "t".into(),
        created_at: 1,
        updated_at: 1,
        ..Default::default()
    };
    conv.messages.push(ChatMessage {
        id: "u0".into(),
        role: MessageRole::User,
        content: "earlier".into(),
        ..Default::default()
    });
    store.save_conversation(&conv).await.unwrap();
}

/// Every file under `root` with its bytes' digest, sequence counters included.
fn tree(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push((
                    p.strip_prefix(base).unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

async fn run(
    provider: Arc<dyn StreamingProvider>,
    storage: Arc<dyn StoragePort>,
    opts: rustain::adapters::cli::ask::AskOpts,
    workspace: &std::path::Path,
) -> (ExitCode, String, String) {
    let core = core_over(provider.clone(), storage);
    let config = default_config();
    let (mut out, mut err) = (Cursor::new(Vec::new()), Cursor::new(Vec::new()));
    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider, core, opts, &config, workspace, &mut out, &mut err,
    )
    .await;
    (
        exit,
        String::from_utf8(out.into_inner()).unwrap(),
        String::from_utf8(err.into_inner()).unwrap(),
    )
}

#[tokio::test]
async fn dry_run_leaves_the_session_tree_and_authority_untouched_even_when_a_tool_is_asked_for() {
    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);
    seeded(&store, "resume-me").await;
    let sessions = dir.path().join(".claude").join("sessions");
    let before = tree(&sessions);
    assert!(!before.is_empty());

    let provider: Arc<dyn StreamingProvider> = Arc::new(Scripted {
        calls: AtomicUsize::new(0),
        ask_for_tool: true,
        competitor: None,
    });
    let mut opts = make_dry_run_opts("plan it", rustain::adapters::cli::ask::OutputFormat::Text);
    opts.session_id = Some("resume-me".into());
    let (exit, out, err) = run(provider, store.clone(), opts, dir.path()).await;
    assert_eq!(exit, ExitCode::SUCCESS, "stderr: {err}");
    assert!(
        out.contains("final answer"),
        "the planning output is unchanged: {out}"
    );
    assert!(!err.contains("Session saved"), "no resume hint: {err}");
    assert_eq!(
        tree(&sessions),
        before,
        "no session, checkpoint, snapshot, sidecar or authority byte changed"
    );
}

#[tokio::test]
async fn dry_run_of_a_new_session_creates_no_session_files_at_all() {
    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);
    let provider: Arc<dyn StreamingProvider> = Arc::new(Scripted {
        calls: AtomicUsize::new(0),
        ask_for_tool: true,
        competitor: None,
    });
    let opts = make_dry_run_opts("plan it", rustain::adapters::cli::ask::OutputFormat::Text);
    let (exit, _, err) = run(provider, store, opts, dir.path()).await;
    assert_eq!(exit, ExitCode::SUCCESS, "stderr: {err}");
    assert!(
        !dir.path().join(".claude").exists(),
        "not even the sessions directory was created"
    );
}

#[tokio::test]
async fn a_saved_run_prints_the_resume_hint_and_persists_the_exchange() {
    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);
    let provider: Arc<dyn StreamingProvider> = Arc::new(Scripted {
        calls: AtomicUsize::new(0),
        ask_for_tool: false,
        competitor: None,
    });
    let (exit, _, err) = run(provider, store.clone(), make_opts("hello"), dir.path()).await;
    assert_eq!(exit, ExitCode::SUCCESS, "stderr: {err}");
    assert!(
        err.contains("Session saved. Resume with: rustain --session"),
        "{err}"
    );
    let listed = store.list_conversations_read_only().await.unwrap();
    assert_eq!(listed.len(), 1);
    let saved = store
        .load_conversation(&listed[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.messages.last().unwrap().content, "final answer");
}

#[tokio::test]
async fn a_final_save_refused_after_a_competing_commit_claims_no_saved_session() {
    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);
    seeded(&store, "resume-me").await;
    let competitor = store_in(&dir);
    let provider: Arc<dyn StreamingProvider> = Arc::new(Scripted {
        calls: AtomicUsize::new(0),
        ask_for_tool: false,
        competitor: Some((competitor.clone(), "resume-me".into())),
    });
    let mut opts = make_opts("continue");
    opts.session_id = Some("resume-me".into());
    let (exit, out, err) = run(provider, store, opts, dir.path()).await;
    assert_eq!(
        exit,
        ExitCode::SUCCESS,
        "the rendered output does not depend on persistence"
    );
    assert!(out.contains("final answer"));
    assert!(err.contains("Warning: failed to save session"), "{err}");
    assert!(
        !err.contains("Session saved"),
        "no resume claim for a refused save: {err}"
    );
    let disk = competitor
        .load_conversation("resume-me")
        .await
        .unwrap()
        .unwrap();
    assert!(
        disk.messages.iter().any(|m| m.id == "a-competitor"),
        "the accepted history stands"
    );
    assert!(
        !disk.messages.iter().any(|m| m.content == "final answer"),
        "the stale answer was not appended"
    );
}
