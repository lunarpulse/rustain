//! Story 19.30: fork at the conversation tail after a plan run.
//!
//! The plan summary row is a durable row pushed by `finish_plan` on the render tick,
//! after the last turn's background save. The fork re-reads storage, so the handler must
//! write the live transcript first. Every keystone enters through the production handlers
//! (`handle_plan_proposed` -> `handle_plan_decision` -> `PlanRuntime` -> the fork decision
//! -> `handle_fork_confirm`) against a real `FileSystemStorage`.

use std::sync::Arc;

use tempfile::TempDir;

use crate::conformance_21_6_decision_queue::support;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{self, DecisionSubject};
use rustain::adapters::tui::handlers::plan_card::{handle_plan_decision, handle_plan_proposed};
use rustain::adapters::tui::handlers::transcript_target::{
    handle_fork_confirm, handle_rewind_fork_instead, verify_target,
};
use rustain::adapters::tui::keymap::table_chat::ChatCmd;
use rustain::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::StorageError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::CheckpointId;
use rustain::domain::models::plan::PlanDecision;
use rustain::domain::models::{
    ChatMessage, ContentBlockType, Conversation, ConversationSummary, MessageRole, PermissionMode,
    Plan, PlanStatus, PlanTask, PlanTaskStatus, SubTaskFailurePolicy, generate_message_id,
};
use rustain::domain::ports::{EventEmitter, StoragePort};
use rustain::domain::services::plan_runtime::{PlanRuntime, TaskTurnOutcome};

// ── Helpers ──────────────────────────────────────────────────────────────────

struct Sink;

impl EventEmitter for Sink {
    fn emit(&self, _event: AppEvent) {}
}

fn row(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        id: generate_message_id(),
        role,
        content: content.to_owned(),
        content_blocks: vec![ContentBlockType::Text],
        created_at: 1_700_000_000,
        ..Default::default()
    }
}

/// An assistant row carrying one completed tool call (copied unchanged by a fork).
fn row_with_tool_call(content: &str) -> ChatMessage {
    let mut message = row(MessageRole::Assistant, content);
    message.tool_calls = vec![
        serde_json::from_value(serde_json::json!({
            "id": "tool-1",
            "name": "read_file",
            "input": {"path": "a.txt"},
            "result": {"content": "ok", "isError": false},
            "startedAtMs": 1,
            "completedAtMs": 2
        }))
        .expect("a tool call"),
    ];
    message
}

fn conversation(id: &str) -> Conversation {
    Conversation {
        id: id.to_owned(),
        title: "Plan run".to_owned(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_001,
        ..Default::default()
    }
}

fn plan() -> Plan {
    Plan {
        id: "plan-1".to_owned(),
        title: "Ship it".to_owned(),
        tasks: vec![PlanTask {
            number: 1,
            title: "Do the work".to_owned(),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        }],
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 1_700_000_000,
        resolved_at: None,
        host_message_id: None,
        origin_restrictions: None,
    }
}

fn store(dir: &TempDir) -> FileSystemStorage {
    FileSystemStorage::new(dir.path().join("sessions"))
}

fn durable_ids(conversation: &Conversation) -> Vec<String> {
    conversation
        .messages
        .iter()
        .filter(|m| !m.is_ephemeral_display())
        .map(|m| m.id.clone())
        .collect()
}

fn ids(conversation: &Conversation) -> Vec<String> {
    conversation.messages.iter().map(|m| m.id.clone()).collect()
}

async fn stored(store: &FileSystemStorage, id: &str) -> Conversation {
    store
        .load_conversation(id)
        .await
        .expect("load")
        .expect("stored conversation")
}

/// A completed plan run: the task turn's save has happened, the plan summary row has
/// not (the render tick pushes it after the save). Returns the live conversation.
async fn plan_run_tail(store: &FileSystemStorage) -> Conversation {
    let mut conv = conversation("plan-run");
    conv.messages.push(row(MessageRole::User, "build it"));
    conv.messages
        .push(row(MessageRole::Assistant, "Here is the plan"));
    let mut state = TuiState::new(80, 24);
    assert!(
        handle_plan_proposed(
            &mut conv,
            &mut state,
            "plan-run",
            plan(),
            PermissionMode::Normal,
            None
        )
        .is_none(),
        "a normal-mode proposal waits for the card decision"
    );
    handle_plan_decision(&mut conv, &mut state, "plan-1", &PlanDecision::Approve);
    let runtime = PlanRuntime::new();
    runtime.clone().start(
        "plan-run".to_owned(),
        "plan-1".to_owned(),
        &mut conv,
        &Sink,
        &[],
        PermissionMode::Normal,
        SubTaskFailurePolicy::default(),
    );
    assert_eq!(
        conv.plans["plan-1"].tasks[0].status,
        PlanTaskStatus::Running
    );
    // The executor's task turn, saved by the turn-complete background save.
    let mut prompt = row(MessageRole::User, "Execute task 1");
    prompt.synthetic = true;
    conv.messages.push(prompt);
    let mut reply = row_with_tool_call("Task 1 done");
    reply.synthetic = true;
    conv.messages.push(reply);
    store
        .save_conversation(&conv)
        .await
        .expect("task-turn save");
    runtime
        .on_turn_complete(
            &"plan-run".to_owned(),
            "plan-1",
            1,
            TaskTurnOutcome::Success {
                result_text: "Task 1 done".to_owned(),
                tool_call_count: 1,
                token_count: Some(10),
            },
            &mut conv,
            &Sink,
            &[],
            PermissionMode::Normal,
        )
        .await;

    let tail = conv.messages.last().expect("tail");
    assert!(
        tail.content_blocks.contains(&ContentBlockType::PlanSummary),
        "precondition: the plan summary is the live tail"
    );
    assert_eq!(conv.plans["plan-1"].status, PlanStatus::Completed);
    let on_disk = stored(store, "plan-run").await;
    assert_eq!(
        on_disk.messages.len(),
        durable_ids(&conv).len() - 1,
        "precondition: the summary is the one durable row not yet saved"
    );
    conv
}

/// The `f` then `y` entry, exactly as the event loop runs it: raise the fork decision on
/// the chosen live row, route `y`, take the payload, verify the target.
fn confirmed_fork_target(conv: &Conversation, live_index: usize) -> usize {
    let (mut state, clock) = support::clocked(80, 24);
    let target = &conv.messages[live_index];
    decision_queue::raise_fork(
        &mut state,
        &conv.id,
        live_index,
        &target.content,
        decision_queue::message_target_fingerprint(target),
    );
    let facts = rustain::adapters::tui::notification_log::LogFacts {
        active_messages: conv.messages.len(),
        ..support::facts(&conv.id)
    };
    support::paint(&mut state, &facts, 80, 24);
    support::ready(&mut state, &clock);
    assert_eq!(
        support::route(&mut state, &support::key('y'), &facts)
            .unwrap()
            .into_action(),
        InputAction::ForkConfirm
    );
    let Some(DecisionSubject::Fork(payload)) = state.decisions.take_resolved() else {
        panic!("confirmed fork payload");
    };
    assert_eq!(payload.conversation_id, conv.id);
    verify_target(conv, payload.message_index, payload.target_hash).expect("target still current");
    payload.message_index
}

async fn fork(
    store: &FileSystemStorage,
    conv: &Conversation,
    live_index: usize,
) -> Result<String, StorageError> {
    handle_fork_confirm(store, &mut None, false, conv, live_index).await
}

/// `f` at the tail: the `auto_snapshot` rule of the event loop selects the last live row.
fn tail(conv: &Conversation) -> usize {
    conv.messages.len() - 1
}

// ── K1 / K4 / K9: after a plan run ───────────────────────────────────────────

#[tokio::test]
async fn k1_tail_fork_after_a_plan_run_holds_the_whole_conversation() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let conv = plan_run_tail(&store).await;
    let live_durable = durable_ids(&conv);
    let index = confirmed_fork_target(&conv, tail(&conv));

    let new_id = fork(&store, &conv, index)
        .await
        .expect("the tail fork after a plan run succeeds");

    let forked = stored(&store, &new_id).await;
    assert_eq!(ids(&forked), live_durable, "every durable row, in order");
    assert!(
        forked
            .messages
            .last()
            .unwrap()
            .content_blocks
            .contains(&ContentBlockType::PlanSummary),
        "the fork ends with the plan summary"
    );
    assert!(forked.messages.iter().all(|m| !m.is_ephemeral_display()));
    let source = forked.fork_source.expect("fork provenance");
    assert_eq!(source.conversation_id, "plan-run");
    assert_eq!(source.message_index, live_durable.len() - 1);
    let reloaded = stored(&store, "plan-run").await;
    assert_eq!(
        ids(&reloaded),
        live_durable,
        "the source file is reconciled"
    );
    assert!(reloaded.messages.iter().all(|m| !m.is_ephemeral_display()));
}

#[tokio::test]
async fn k4_fork_before_the_plan_copies_the_completed_plan() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let conv = plan_run_tail(&store).await;
    // The assistant row that proposed the plan (live index 1).
    let index = confirmed_fork_target(&conv, 1);

    let new_id = fork(&store, &conv, index).await.expect("fork");

    let forked = stored(&store, &new_id).await;
    assert_eq!(forked.messages.len(), 2);
    let copied = &forked.plans["plan-1"];
    assert_eq!(
        copied.status,
        PlanStatus::Completed,
        "not the stored Executing"
    );
    assert_eq!(copied.tasks[0].status, PlanTaskStatus::Completed);
}

#[tokio::test]
async fn k9_rewind_fork_instead_at_the_tail_forks_like_the_fork_key() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let conv = plan_run_tail(&store).await;
    let live_durable = durable_ids(&conv);

    let new_id = handle_rewind_fork_instead(&store, &mut None, false, &conv, tail(&conv))
        .await
        .expect("rewind-fork-instead at the tail succeeds");

    let forked = stored(&store, &new_id).await;
    assert_eq!(ids(&forked), live_durable);
}

// ── K2 / K3: other persisted-count lags ──────────────────────────────────────

#[tokio::test]
async fn k2_tail_fork_without_a_plan_run_copies_tool_calls_unchanged() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let mut conv = conversation("saved");
    conv.messages.push(row(MessageRole::User, "hello"));
    conv.messages.push(row_with_tool_call("done"));
    store.save_conversation(&conv).await.unwrap();
    let index = confirmed_fork_target(&conv, tail(&conv));

    let new_id = fork(&store, &conv, index).await.expect("fork");

    let forked = stored(&store, &new_id).await;
    assert_eq!(ids(&forked), ids(&conv));
    // Loading assigns a fresh tool-call id, so the copy is compared without it.
    let without_ids = |message: &ChatMessage| {
        let mut calls = serde_json::to_value(&message.tool_calls).unwrap();
        for call in calls.as_array_mut().unwrap() {
            call.as_object_mut().unwrap().remove("id");
        }
        calls
    };
    assert_eq!(
        without_ids(&forked.messages[1]),
        without_ids(&conv.messages[1]),
        "tool calls are copied unchanged"
    );
}

/// Generic lag case: any durable row present in memory but not yet saved is forkable.
#[tokio::test]
async fn k3_an_unsaved_durable_tail_row_is_forkable() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let mut conv = conversation("lagging");
    conv.messages.push(row(MessageRole::User, "hello"));
    conv.messages.push(row(MessageRole::Assistant, "hi"));
    store.save_conversation(&conv).await.unwrap();
    conv.messages
        .push(row(MessageRole::Assistant, "not saved yet"));
    let index = confirmed_fork_target(&conv, tail(&conv));

    let new_id = fork(&store, &conv, index).await.expect("fork");

    let forked = stored(&store, &new_id).await;
    assert_eq!(ids(&forked), ids(&conv));
    assert_eq!(forked.messages.last().unwrap().content, "not saved yet");
}

// ── K5: a temporary tail row is still refused, and nothing is written ────────

#[tokio::test]
async fn k5_a_temporary_tail_row_is_refused_before_any_write() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let mut conv = conversation("temporary");
    conv.messages.push(row(MessageRole::User, "hello"));
    conv.messages.push(row(MessageRole::Assistant, "hi"));
    let mut state = TuiState::new(80, 24);
    handle_plan_proposed(
        &mut conv,
        &mut state,
        "temporary",
        plan(),
        PermissionMode::Normal,
        None,
    );
    store.save_conversation(&conv).await.unwrap();
    // A durable row that is only in memory: a flush before the refusal would show.
    conv.messages.push(row(MessageRole::Assistant, "unsaved"));
    conv.messages
        .push(row(MessageRole::Assistant, "temporary tail"));
    conv.messages
        .last_mut()
        .unwrap()
        .content_blocks
        .push(ContentBlockType::EphemeralDisplay);
    let session_file = dir.path().join("sessions").join("temporary.meta.json");
    let before = std::fs::read(&session_file).expect("the saved session file");
    let count = store.list_conversations().await.unwrap().len();

    let refused = fork(&store, &conv, tail(&conv)).await;

    assert_eq!(
        refused.unwrap_err().to_string(),
        StorageError::Other("Temporary display rows are not persistent fork/rewind targets".into())
            .to_string()
    );
    assert_eq!(
        std::fs::read(&session_file).expect("the session file"),
        before,
        "the refusal leaves the session file byte-for-byte unchanged"
    );
    assert_eq!(store.list_conversations().await.unwrap().len(), count);
}

// ── K8: the attached host has no fork key ────────────────────────────────────

#[test]
fn k8_f_forks_on_the_local_host_only() {
    let forks = |host| {
        matches!(
            keymap::resolve(Context::Chat, host, &[Key::Char('f')]),
            Resolved::Hit(row) if matches!(row.command, CommandId::Chat(ChatCmd::ForkAtMessage))
        )
    };
    assert!(forks(Host::Local));
    assert!(!forks(Host::Attached));
}

// ── K6: an in-flight save is drained before the flush and the fork ───────────

#[derive(Debug, PartialEq)]
enum Record {
    Save(usize),
    Fork(u64),
}

/// Its async methods contain no `.await`, so the only yield point in the handler is
/// the drain of the in-flight save.
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<Record>>);

#[async_trait::async_trait]
impl StoragePort for Recorder {
    async fn save_conversation(&self, conv: &Conversation) -> Result<(), StorageError> {
        self.0
            .lock()
            .unwrap()
            .push(Record::Save(conv.messages.len()));
        Ok(())
    }

    async fn load_conversation(&self, _id: &str) -> Result<Option<Conversation>, StorageError> {
        Ok(None)
    }

    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>, StorageError> {
        Ok(Vec::new())
    }

    async fn read_snapshot(
        &self,
        _conversation_id: &str,
        _checkpoint: CheckpointId,
        _path: &std::path::Path,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(None)
    }

    async fn fork_at_checkpoint(
        &self,
        _source_conversation_id: &str,
        checkpoint: CheckpointId,
    ) -> Result<String, StorageError> {
        self.0.lock().unwrap().push(Record::Fork(checkpoint.0));
        Ok("forked".to_owned())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn k6_the_in_flight_save_lands_before_the_flush_and_the_fork() {
    let recorder = Arc::new(Recorder::default());
    let mut live = conversation("ordering");
    live.messages.push(row(MessageRole::User, "one"));
    live.messages.push(row(MessageRole::Assistant, "two"));
    live.messages.push(row(MessageRole::Assistant, "three"));
    let mut stale = live.clone();
    stale.messages.pop();
    // A spawned task cannot run until this task yields.
    let background = recorder.clone();
    let mut pending = Some(tokio::spawn(async move {
        background.save_conversation(&stale).await.unwrap();
    }));

    let forked = handle_fork_confirm(&*recorder, &mut pending, false, &live, 2)
        .await
        .expect("fork");

    assert_eq!(forked, "forked");
    assert_eq!(
        *recorder.0.lock().unwrap(),
        [Record::Save(2), Record::Save(3), Record::Fork(2)],
        "stale save, then the live flush, then the fork"
    );
    assert!(pending.is_none(), "the in-flight save was taken");
}

// ── K10: no source write while a turn is running ─────────────────────────────

/// A running turn saves the same source on its own (outside `pending_save`), through the
/// same temporary path. The confirmation therefore refuses before it drains or writes.
#[tokio::test(flavor = "current_thread")]
async fn k10_a_fork_confirmed_while_a_turn_runs_writes_nothing_and_says_what_to_do() {
    let recorder = Arc::new(Recorder::default());
    let mut live = conversation("running");
    live.messages.push(row(MessageRole::User, "one"));
    live.messages.push(row(MessageRole::Assistant, "two"));
    let background = recorder.clone();
    let mut stale = live.clone();
    stale.messages.pop();
    let mut pending = Some(tokio::spawn(async move {
        background.save_conversation(&stale).await.unwrap();
    }));

    for entry in 0..2 {
        let refused = if entry == 0 {
            handle_fork_confirm(&*recorder, &mut pending, true, &live, 1).await
        } else {
            handle_rewind_fork_instead(&*recorder, &mut pending, true, &live, 1).await
        };
        let message = refused
            .expect_err("refused while a turn is running")
            .to_string();
        assert!(
            message.contains("response is still running") && message.contains("fork again"),
            "{message}"
        );
    }

    assert!(recorder.0.lock().unwrap().is_empty(), "no write, no fork");
    assert!(pending.is_some(), "the in-flight save is left alone");
    let idle = handle_fork_confirm(&*recorder, &mut pending, false, &live, 1).await;
    assert_eq!(idle.expect("fork once the turn is done"), "forked");
    assert_eq!(
        *recorder.0.lock().unwrap(),
        [Record::Save(1), Record::Save(2), Record::Fork(1)]
    );
}
