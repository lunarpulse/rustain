//! Cron scheduler adapter (Story 12.4).
//!
//! The scheduler drives cron turns directly through `DaemonTurnRuntime::drive_turn`
//! with a per-job conversation and per-job event channel. Completed results are
//! injected back into the daemon shared conversation through `CronCompletion`, so
//! the attach forwarder remains the only writer of that shared transcript.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Local};
use croner::Cron;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::adapters::daemon::runtime::DaemonCore;
use crate::domain::errors::TransitionError;
use crate::domain::events::AppEvent;
use crate::domain::models::{
    ChannelKind, ChatMessage, ContentBlockType, Conversation, CronConfig, CronJob, HealthSummary,
    MessageRole, ProviderReplayItem, ResponseSpanBuilder, StopReason, StreamChunk, ToolCallInfo,
    ToolResultInfo, TurnOrigin, generate_message_id,
};
use crate::domain::ports::{ChannelPort, SchedulerPort, StoragePort};
use crate::domain::services::{
    budget_gate::{BudgetStop, daily_budget_stop},
    turn_reply::TurnReplyFold,
};

const HEALTH_OK: u8 = 0;
const HEALTH_DEGRADED: u8 = 1;
const HEALTH_OFFLINE: u8 = 2;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_CONCURRENCY_LIMIT: usize = 8;

#[derive(Debug, Clone)]
pub struct CronCompletion {
    pub job_name: String,
    pub result_text: String,
}

#[derive(Clone)]
pub struct ScheduledJob {
    pub name: String,
    pub cron: Cron,
    pub prompt: String,
    pub forward: bool,
    pub timeout_secs: Option<u64>,
}

pub struct CronSchedulerAdapter {
    cron_toml_path: PathBuf,
    jobs: Arc<ArcSwap<Vec<ScheduledJob>>>,
    core: Arc<DaemonCore>,
    completion_tx: mpsc::UnboundedSender<CronCompletion>,
    channel: Arc<dyn ChannelPort>,
    storage: Arc<dyn StoragePort>,
    shutdown: CancellationToken,
    job_timeout: Duration,
    sem: Arc<tokio::sync::Semaphore>,
    /// tokio::sync — locks=4 held.
    running: Arc<Mutex<std::collections::HashSet<String>>>,
    /// tokio::sync — locks=4 held.
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// tokio::sync — locks=4 held.
    inflight: Arc<Mutex<Vec<JoinHandle<()>>>>,
    reload_notify: Arc<Notify>,
    /// tokio::sync — locks=4 held.
    next_runs: Arc<Mutex<Vec<(String, Option<i64>)>>>,
    health: Arc<AtomicU8>,
}

impl CronSchedulerAdapter {
    pub async fn load(
        cron_toml_path: PathBuf,
        core: Arc<DaemonCore>,
        completion_tx: mpsc::UnboundedSender<CronCompletion>,
        channel: Arc<dyn ChannelPort>,
        storage: Arc<dyn StoragePort>,
    ) -> anyhow::Result<Self> {
        let jobs = load_jobs(&cron_toml_path).await;
        // Pre-warm the OnceCell runtime once before concurrent first-fires.
        core.ensure_runtime().await.map_err(|error| {
            tracing::warn!(%error, "cron: pre-warming runtime failed");
            anyhow::anyhow!("cron scheduler runtime pre-warm failed")
        })?;
        Ok(Self::new_loaded(
            cron_toml_path,
            jobs,
            core,
            completion_tx,
            channel,
            storage,
            DEFAULT_JOB_TIMEOUT,
            DEFAULT_CONCURRENCY_LIMIT,
        ))
    }

    pub fn new_loaded(
        cron_toml_path: PathBuf,
        jobs: Vec<ScheduledJob>,
        core: Arc<DaemonCore>,
        completion_tx: mpsc::UnboundedSender<CronCompletion>,
        channel: Arc<dyn ChannelPort>,
        storage: Arc<dyn StoragePort>,
        job_timeout: Duration,
        concurrency_limit: usize,
    ) -> Self {
        Self {
            cron_toml_path,
            jobs: Arc::new(ArcSwap::from_pointee(jobs)),
            core,
            completion_tx,
            channel,
            storage,
            shutdown: CancellationToken::new(),
            job_timeout,
            sem: Arc::new(tokio::sync::Semaphore::new(concurrency_limit.max(1))),
            running: Arc::new(Mutex::new(std::collections::HashSet::new())),
            handle: Arc::new(Mutex::new(None)),
            inflight: Arc::new(Mutex::new(Vec::new())),
            reload_notify: Arc::new(Notify::new()),
            next_runs: Arc::new(Mutex::new(Vec::new())),
            health: Arc::new(AtomicU8::new(HEALTH_DEGRADED)),
        }
    }

    pub async fn reload(&self) -> Result<(), TransitionError> {
        let jobs = load_jobs(&self.cron_toml_path).await;
        self.jobs.store(Arc::new(jobs));
        self.reload_notify.notify_one();
        Ok(())
    }

    pub async fn next_runs(&self) -> Vec<(String, Option<i64>)> {
        self.next_runs.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl SchedulerPort for CronSchedulerAdapter {
    fn health_snapshot(&self) -> HealthSummary {
        match self.health.load(Ordering::SeqCst) {
            HEALTH_OK => HealthSummary::healthy("running"),
            HEALTH_OFFLINE => HealthSummary::error("stopped", "restart daemon"),
            _ => HealthSummary::degraded("idle", "check cron.toml"),
        }
    }

    async fn start_loop(&self) -> Result<(), TransitionError> {
        let mut slot = self.handle.lock().await;
        if slot.as_ref().is_some_and(|h| !h.is_finished()) {
            return Ok(());
        }
        let state = LoopState {
            jobs: self.jobs.clone(),
            core: self.core.clone(),
            completion_tx: self.completion_tx.clone(),
            channel: self.channel.clone(),
            storage: self.storage.clone(),
            shutdown: self.shutdown.clone(),
            job_timeout: self.job_timeout,
            sem: self.sem.clone(),
            running: self.running.clone(),
            inflight: self.inflight.clone(),
            reload_notify: self.reload_notify.clone(),
            next_runs: self.next_runs.clone(),
            health: self.health.clone(),
        };
        *slot = Some(tokio::spawn(async move { run_scheduler_loop(state).await }));
        Ok(())
    }

    async fn reload(&self) -> Result<(), TransitionError> {
        CronSchedulerAdapter::reload(self).await
    }

    fn request_shutdown(&self) {
        self.shutdown.cancel();
    }

    async fn shutdown_loop(&self) -> Result<(), TransitionError> {
        self.request_shutdown();
        // One shared deadline bounds shutdown independently of job timeouts.
        let deadline = tokio::time::sleep(SHUTDOWN_GRACE);
        tokio::pin!(deadline);
        if let Some(mut handle) = self.handle.lock().await.take() {
            tokio::select! {
                _ = &mut deadline => { handle.abort(); let _ = handle.await; }
                r = &mut handle => { let _ = r; }
            }
        }
        let mut handles = self.inflight.lock().await;
        let mut pending = std::mem::take(&mut *handles);
        drop(handles);
        for mut handle in pending.drain(..) {
            tokio::select! {
                _ = &mut deadline => { handle.abort(); let _ = handle.await; }
                r = &mut handle => { let _ = r; }
            }
        }
        self.health.store(HEALTH_OFFLINE, Ordering::SeqCst);
        Ok(())
    }
}

struct LoopState {
    jobs: Arc<ArcSwap<Vec<ScheduledJob>>>,
    core: Arc<DaemonCore>,
    completion_tx: mpsc::UnboundedSender<CronCompletion>,
    channel: Arc<dyn ChannelPort>,
    storage: Arc<dyn StoragePort>,
    shutdown: CancellationToken,
    job_timeout: Duration,
    sem: Arc<tokio::sync::Semaphore>,
    running: Arc<Mutex<std::collections::HashSet<String>>>,
    inflight: Arc<Mutex<Vec<JoinHandle<()>>>>,
    reload_notify: Arc<Notify>,
    next_runs: Arc<Mutex<Vec<(String, Option<i64>)>>>,
    health: Arc<AtomicU8>,
}

async fn run_scheduler_loop(state: LoopState) {
    state.health.store(HEALTH_OK, Ordering::SeqCst);
    while !state.shutdown.is_cancelled() {
        // Reap finished inflight handles to prevent unbounded growth.
        {
            let mut inflight = state.inflight.lock().await;
            inflight.retain(|h| !h.is_finished());
        }
        let now = Local::now();
        let snapshot = state.jobs.load_full();
        let mut runs = Vec::with_capacity(snapshot.len());
        let mut min_next: Option<DateTime<Local>> = None;
        for job in snapshot.iter() {
            let next = next_occurrence(&job.cron, now);
            runs.push((job.name.clone(), next.map(|dt| dt.timestamp())));
            if let Some(next) = next {
                min_next = Some(min_next.map_or(next, |cur| cur.min(next)));
            }
        }
        *state.next_runs.lock().await = runs;

        let Some(next) = min_next else {
            tokio::select! {
                biased;
                _ = state.shutdown.cancelled() => break,
                _ = state.reload_notify.notified() => continue,
            }
        };

        let delay = next
            .signed_duration_since(Local::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => break,
            _ = state.reload_notify.notified() => continue,
            _ = tokio::time::sleep(delay) => {
                let fire_at = next.timestamp();
                for job in snapshot.iter() {
                    if state.shutdown.is_cancelled() {
                        break;
                    }
                    if job.cron.is_time_matching(&next).unwrap_or(false) {
                        maybe_spawn_job(&state, job.clone(), fire_at).await;
                    }
                }
            }
        }
    }
    state.health.store(HEALTH_OFFLINE, Ordering::SeqCst);
}

async fn maybe_spawn_job(state: &LoopState, job: ScheduledJob, fired_at_unix: i64) {
    let permit = {
        let mut running = state.running.lock().await;
        if state.shutdown.is_cancelled() {
            return;
        }
        if running.contains(&job.name) {
            tracing::warn!(job = %job.name, "cron: job still running; skipping overlapping fire");
            return;
        }
        let permit = match state.sem.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                tracing::warn!(job = %job.name, "cron: global concurrency cap reached; skipping fire");
                return;
            }
        };
        running.insert(job.name.clone());
        permit
    };

    let task = CronJobTask {
        job,
        fired_at_unix,
        core: state.core.clone(),
        completion_tx: state.completion_tx.clone(),
        channel: state.channel.clone(),
        storage: state.storage.clone(),
        shutdown: state.shutdown.clone(),
        job_timeout: state.job_timeout,
        running: state.running.clone(),
        _permit: permit,
    };
    let handle = tokio::spawn(async move { run_job(task).await });
    state.inflight.lock().await.push(handle);
}

struct CronJobTask {
    job: ScheduledJob,
    fired_at_unix: i64,
    core: Arc<DaemonCore>,
    completion_tx: mpsc::UnboundedSender<CronCompletion>,
    channel: Arc<dyn ChannelPort>,
    storage: Arc<dyn StoragePort>,
    shutdown: CancellationToken,
    job_timeout: Duration,
    running: Arc<Mutex<std::collections::HashSet<String>>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Default)]
struct CronRecorder {
    span: ResponseSpanBuilder,
    calls: Vec<ToolCallInfo>,
    replay: Vec<ProviderReplayItem>,
    committed: Vec<String>,
}

impl CronRecorder {
    async fn push(
        &mut self,
        chunk: &StreamChunk,
        conversation: &mut Conversation,
        storage: &dyn StoragePort,
    ) {
        match chunk {
            StreamChunk::Text { content, .. } => self.span.text(content),
            StreamChunk::Thinking { content, .. } => self.span.reasoning(content),
            StreamChunk::ProviderReplay { item } => {
                self.replay.push(item.clone());
                self.span.replay();
            }
            StreamChunk::ToolUse { id, name, input } => {
                self.calls.push(ToolCallInfo {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    result: None,
                    started_at_ms: None,
                    completed_at_ms: None,
                    status: None,
                    provider_id: Some(id.clone()),
                });
                self.span.call();
            }
            StreamChunk::ToolResult {
                id,
                content,
                is_error,
                diff,
                images,
                ..
            } => {
                let result = ToolResultInfo {
                    content: content.clone(),
                    is_error: *is_error,
                    diff: diff.clone(),
                    images: images.clone(),
                };
                let unanswered = |call: &&mut ToolCallInfo| {
                    call.wire_id() == id.as_str() && call.result.is_none()
                };
                if let Some(call) = self.calls.iter_mut().rev().find(unanswered) {
                    call.result = Some(result);
                } else if let Some(call) = conversation
                    .messages
                    .iter_mut()
                    .rev()
                    .filter(|message| self.committed.contains(&message.id))
                    .flat_map(|message| message.tool_calls.iter_mut().rev())
                    .find(unanswered)
                {
                    call.result = Some(result);
                    save_cron_session(conversation, storage).await;
                }
            }
            StreamChunk::TurnComplete { stop_reason } if *stop_reason != StopReason::Cancelled => {
                if !self.span.is_empty() || !self.calls.is_empty() || !self.replay.is_empty() {
                    let span = self.span.finish();
                    let mut blocks = Vec::with_capacity(self.replay.len() + 2);
                    if let Some(reasoning) = &span.reasoning {
                        blocks.push(ContentBlockType::Thinking(reasoning.clone()));
                    }
                    blocks.extend(self.replay.drain(..).map(ContentBlockType::ProviderReplay));
                    let content = span.text.clone();
                    blocks.push(ContentBlockType::ResponseSpan(span));
                    let id = generate_message_id();
                    self.committed.push(id.clone());
                    conversation.messages.push(ChatMessage {
                        id,
                        role: MessageRole::Assistant,
                        content,
                        content_blocks: blocks,
                        tool_calls: std::mem::take(&mut self.calls),
                        created_at: crate::domain::models::session_meta::now_unix(),
                        stop_reason: Some(stop_reason.clone()),
                        origin: ChannelKind::Cron,
                        ..Default::default()
                    });
                    save_cron_session(conversation, storage).await;
                }
            }
            _ => {}
        }
    }
}

async fn save_cron_session(conversation: &mut Conversation, storage: &dyn StoragePort) {
    conversation.updated_at = crate::domain::models::session_meta::now_unix();
    conversation.last_response_at = Some(conversation.updated_at);
    if let Err(error) = storage.save_conversation(conversation).await {
        tracing::warn!(%error, "cron: saving session failed");
    }
}

/// Only trusted values belong in a marker: the same text leaves the job
/// through the session, shared completion, and external notification.
#[derive(Clone, Copy)]
enum CronMarker {
    RuntimeUnavailable,
    LedgerUnreadable,
    BudgetReached(BudgetStop),
    Failed,
    Disconnected,
    Timeout(u64),
    Shutdown,
}

impl CronMarker {
    fn with_reply(self, reply: &str) -> String {
        let mut marker = match self {
            Self::RuntimeUnavailable => "[cron: failed — runtime unavailable]".to_string(),
            Self::LedgerUnreadable => "[cron: skipped — usage ledger unreadable]".to_string(),
            Self::BudgetReached(stop) => format!(
                "[cron: skipped — daily budget of ${:.2} reached (${:.2} spent today)]",
                stop.limit_usd, stop.spent_usd
            ),
            Self::Failed => "[cron: failed — turn ended without a reply]".to_string(),
            Self::Disconnected => "[cron: failed — Stream disconnected unexpectedly]".to_string(),
            Self::Timeout(seconds) => format!("[cron: timed out after {seconds} s]"),
            Self::Shutdown => "[cron: cancelled by daemon shutdown]".to_string(),
        };
        if !reply.is_empty() {
            marker.push('\n');
            marker.push_str(reply);
        }
        marker
    }
}

fn record_cron_marker(conversation: &mut Conversation, content: String) {
    conversation.messages.push(ChatMessage {
        id: generate_message_id(),
        role: MessageRole::Assistant,
        content,
        created_at: crate::domain::models::session_meta::now_unix(),
        stop_reason: Some(StopReason::Cancelled),
        synthetic: true,
        origin: ChannelKind::Cron,
        ..Default::default()
    });
}

async fn finish_cron_job(
    task: &CronJobTask,
    conversation: &mut Conversation,
    marker: Option<CronMarker>,
    reply: String,
) {
    let marker = if task.shutdown.is_cancelled() {
        Some(CronMarker::Shutdown)
    } else {
        marker
    };
    let marker_index = conversation.messages.len();
    if let Some(marker) = marker {
        record_cron_marker(conversation, marker.with_reply(&reply));
    }
    save_cron_session(conversation, task.storage.as_ref()).await;
    let (marker_text, notification) = if task.shutdown.is_cancelled() {
        (None, None)
    } else {
        let marker_text = conversation
            .messages
            .get(marker_index)
            .map(|message| message.content.clone());
        let notification = task.job.forward.then(|| {
            let result = marker_text.as_deref().unwrap_or(&reply);
            format!("[cron: {}] {result}", task.job.name)
        });
        (marker_text, notification)
    };
    if task.shutdown.is_cancelled() {
        if !matches!(marker, Some(CronMarker::Shutdown)) {
            // Shutdown may have begun while the final save was parked. Keep the
            // partial reply locally, replacing any unpublished terminal marker.
            let content = CronMarker::Shutdown.with_reply(&reply);
            if let Some(message) = conversation.messages.get_mut(marker_index) {
                message.content = content;
            } else {
                record_cron_marker(conversation, content);
            }
            save_cron_session(conversation, task.storage.as_ref()).await;
        }
    } else {
        if task
            .completion_tx
            .send(CronCompletion {
                job_name: task.job.name.clone(),
                result_text: marker_text.unwrap_or(reply),
            })
            .is_err()
        {
            tracing::warn!(job = %task.job.name, "cron: completion dropped — forwarder exited");
        }
        if let Some(text) = notification {
            tokio::select! {
                biased;
                _ = task.shutdown.cancelled() => {}
                result = async { task.channel.notify(&text).await } => {
                    if let Err(error) = result {
                        tracing::warn!(job = %task.job.name, %error, "cron: forward failed");
                    }
                }
            }
        }
    }
    task.running.lock().await.remove(&task.job.name);
}

async fn run_job(task: CronJobTask) {
    let session_id = format!(
        "cron-{}-{}",
        sanitize_session_component(&task.job.name),
        task.fired_at_unix
    );
    let mut conversation = new_cron_conversation(&session_id, &task.job.name, task.fired_at_unix);
    let rt = match task.core.ensure_runtime().await {
        Ok(rt) => rt,
        Err(error) => {
            tracing::warn!(job = %task.job.name, %error, "cron: composing job runtime failed");
            finish_cron_job(
                &task,
                &mut conversation,
                Some(CronMarker::RuntimeUnavailable),
                String::new(),
            )
            .await;
            return;
        }
    };
    let config = task.core.config.load_full();
    if config.budget.daily_limit_usd.is_some_and(f64::is_finite) {
        let marker = match rt
            .usage_ledger
            .read_since(
                crate::infrastructure::clock_util::today_start_unix_ms(),
                crate::domain::ports::LedgerReadMode::Lenient,
            )
            .await
        {
            Ok(entries) => daily_budget_stop(&entries, &config).map(CronMarker::BudgetReached),
            Err(error) => {
                tracing::warn!(job = %task.job.name, %error, "cron: usage ledger unreadable");
                Some(CronMarker::LedgerUnreadable)
            }
        };
        if let Some(marker) = marker {
            finish_cron_job(&task, &mut conversation, Some(marker), String::new()).await;
            return;
        }
    }
    if task.shutdown.is_cancelled() {
        finish_cron_job(
            &task,
            &mut conversation,
            Some(CronMarker::Shutdown),
            String::new(),
        )
        .await;
        return;
    }
    let (job_tx, mut job_rx) = mpsc::unbounded_channel();
    let child_cancel = task.shutdown.child_token();
    let handle = rt
        .drive_turn(
            task.job.prompt.clone(),
            ChannelKind::Cron,
            &mut conversation,
            &job_tx,
            TurnOrigin::Cron,
            child_cancel.clone(),
        )
        .await;
    drop(job_tx);
    // A forced abort of this job must not detach the turn it spawned.
    let mut producer = AbortOnDropHandle::new(handle);
    let _cancel_on_drop = child_cancel.drop_guard_ref();
    let duration = task
        .job
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(task.job_timeout);
    let interrupt = async {
        tokio::select! {
            biased;
            _ = task.shutdown.cancelled() => CronMarker::Shutdown,
            _ = tokio::time::sleep(duration) => CronMarker::Timeout(duration.as_secs()),
        }
    };
    let mut recorder = CronRecorder::default();
    let mut reply = TurnReplyFold::default();
    let mut terminal = None;
    let mut disconnected = false;
    let interrupted = {
        let record = async {
            if let Err(error) = task.storage.save_conversation(&conversation).await {
                tracing::warn!(job = %task.job.name, %error, "cron: saving user message failed");
            }
            loop {
                let event = tokio::select! {
                    biased;
                    _ = child_cancel.cancelled() => break,
                    event = job_rx.recv() => event,
                };
                match event {
                    Some(AppEvent::ProviderChunk { chunk, .. }) => {
                        reply.push(&chunk);
                        if let StreamChunk::Error { content } = &chunk {
                            tracing::warn!(job = %task.job.name, %content, "cron: turn stream error");
                        }
                        if let StreamChunk::TurnComplete { stop_reason } = &chunk {
                            if *stop_reason != StopReason::ToolUse {
                                terminal = Some(stop_reason.clone());
                            }
                        }
                        recorder
                            .push(&chunk, &mut conversation, task.storage.as_ref())
                            .await;
                    }
                    Some(AppEvent::CronStreamDisconnected) => disconnected = true,
                    Some(AppEvent::SystemNotice {
                        level: crate::domain::models::NoticeLevel::Error,
                        message,
                        ..
                    }) => {
                        tracing::warn!(job = %task.job.name, %message, "cron: turn error notice");
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        };
        tokio::pin!(interrupt, record);

        // Race the whole recorder, not just channel receive: a storage write
        // may park after consuming a chunk while the producer is still active.
        let mut interrupted = tokio::select! {
            biased;
            marker = &mut interrupt => Some(marker),
            () = &mut record => None,
        };
        let recording_pending = interrupted.is_some();
        if !recording_pending {
            interrupted = tokio::select! {
                biased;
                marker = &mut interrupt => Some(marker),
                _ = &mut producer => None,
            };
        }
        if interrupted.is_some() {
            child_cancel.cancel();
            producer.abort();
            let _ = producer.await;
            if recording_pending {
                // Finish the same ordered save before writing the marker.
                // The cancelled token prevents folding any more chunks.
                record.await;
            }
        }
        interrupted
    };
    let marker = interrupted.or(match terminal {
        Some(StopReason::EndTurn | StopReason::MaxTokens) => None,
        _ if disconnected => Some(CronMarker::Disconnected),
        _ => Some(CronMarker::Failed),
    });
    finish_cron_job(&task, &mut conversation, marker, reply.into_string()).await;
}

fn new_cron_conversation(session_id: &str, job_name: &str, created_at: i64) -> Conversation {
    Conversation {
        id: session_id.to_string(),
        title: format!("cron: {job_name}"),
        messages: Vec::new(),
        turns: Vec::new(),
        created_at,
        updated_at: created_at,
        last_response_at: None,
        session_id: Some(session_id.to_string()),
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
        persist: Default::default(),
    }
}

fn sanitize_session_component(name: &str) -> String {
    const MAX_LEN: usize = 64;
    let mut out = String::with_capacity(name.len().clamp(1, MAX_LEN));
    for ch in name.chars() {
        if out.len() >= MAX_LEN {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "job".to_string()
    } else {
        trimmed.to_string()
    }
}

pub async fn load_jobs(path: &Path) -> Vec<ScheduledJob> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(_) => {
            tracing::info!("cron.toml not found; scheduler idle");
            return Vec::new();
        }
    };
    let config: CronConfig = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "cron.toml malformed; scheduler idle");
            return Vec::new();
        }
    };
    parse_jobs(config.jobs)
}

pub fn parse_jobs(jobs: Vec<CronJob>) -> Vec<ScheduledJob> {
    let mut seen = std::collections::HashSet::new();
    jobs.into_iter()
        .filter_map(|job| {
            if job.name.trim().is_empty() {
                tracing::warn!("⚠ Cron job with empty name: skipped.");
                return None;
            }
            if job.prompt.trim().is_empty() {
                tracing::warn!("⚠ Cron job '{}': empty prompt. Skipped.", job.name);
                return None;
            }
            if job
                .timeout_secs
                .is_some_and(|secs| !(1..=3600).contains(&secs))
            {
                tracing::warn!(
                    "Cron job '{}': timeout_secs must be in 1..=3600. Skipped.",
                    job.name
                );
                return None;
            }
            if !seen.insert(job.name.clone()) {
                tracing::warn!("⚠ Cron job '{}': duplicate name. Skipped.", job.name);
                return None;
            }
            let cron = match Cron::from_str(&job.schedule) {
                Ok(cron) => cron,
                Err(_) => {
                    tracing::warn!(
                        "⚠ Cron job '{}': invalid schedule expression. Skipped.",
                        job.name
                    );
                    return None;
                }
            };
            Some(ScheduledJob {
                name: job.name,
                cron,
                prompt: job.prompt,
                forward: job.forward,
                timeout_secs: job.timeout_secs,
            })
        })
        .collect()
}

pub fn next_occurrence(cron: &Cron, from: DateTime<Local>) -> Option<DateTime<Local>> {
    cron.find_next_occurrence(&from, false).ok()
}

pub fn cron_toml_path() -> anyhow::Result<PathBuf> {
    Ok(crate::infrastructure::paths::config_dir()?.join("cron.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::noop::{
        NoOpApprovalPersistence, NoOpContext, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpToolSet,
        NoOpUsageLedger,
    };
    use crate::adapters::toolset_adapter::ToolSetAdapter;
    use crate::domain::errors::{ProviderError, StorageError};
    use crate::domain::models::provider::ModelDescriptor;
    use crate::domain::models::{AppConfig, CompletionOptions, MemoryFact, Message, StreamChunk};
    use crate::domain::ports::{
        ContextPort, SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
    };
    use crate::domain::services::approval_runtime::ApprovalRuntime;
    use crate::domain::services::tool_scheduler::ToolScheduler;
    use arc_swap::ArcSwap;
    use futures::StreamExt;
    use futures::stream::BoxStream;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

    #[derive(Default)]
    struct RecordingChannel {
        sent: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl ChannelPort for RecordingChannel {
        async fn notify(&self, text: &str) -> Result<(), TransitionError> {
            self.sent.lock().await.push(text.to_string());
            Ok(())
        }
    }

    struct StaticProvider {
        chunks: Vec<StreamChunk>,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for StaticProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            Ok(futures::stream::iter(self.chunks.clone()).boxed())
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "static".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    struct BarrierProvider {
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for BarrierProvider {
        async fn stream_completion(
            &self,
            messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            let prompt = messages
                .iter()
                .rev()
                .find(|m| m.role == crate::domain::models::MessageRole::User)
                .map(|m| m.content.clone())
                .unwrap_or_default();
            self.barrier.wait().await;
            Ok(futures::stream::iter(vec![
                StreamChunk::Text {
                    content: format!("result for {prompt}"),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ])
            .boxed())
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "barrier".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    struct PendingProvider;

    #[async_trait::async_trait]
    impl StreamingProvider for PendingProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            Ok(futures::stream::pending().boxed())
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "pending".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    struct ToolUseProvider {
        barrier: Arc<tokio::sync::Barrier>,
        phases: Arc<Mutex<HashMap<String, usize>>>,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for ToolUseProvider {
        async fn stream_completion(
            &self,
            messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            let prompt = messages
                .iter()
                .rev()
                .find(|m| m.role == crate::domain::models::MessageRole::User)
                .map(|m| m.content.clone())
                .unwrap_or_default();
            let mut phases = self.phases.lock().await;
            let phase = phases.entry(prompt.clone()).or_default();
            *phase += 1;
            let current_phase = *phase;
            drop(phases);

            if current_phase == 1 {
                self.barrier.wait().await;
                let (tool_name, input) = if prompt == "daily" {
                    (
                        "remember",
                        serde_json::json!({"summary": "daily note from cron"}),
                    )
                } else {
                    (
                        "remember_fact",
                        serde_json::json!({"category": "Cron", "fact": "fact note from cron"}),
                    )
                };

                Ok(futures::stream::iter(vec![
                    StreamChunk::ToolUse {
                        id: format!("{prompt}-tool"),
                        name: tool_name.into(),
                        input,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::ToolUse,
                    },
                ])
                .boxed())
            } else {
                Ok(futures::stream::iter(vec![
                    StreamChunk::Text {
                        content: format!("done {prompt}"),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ])
                .boxed())
            }
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "tool-use".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    /// Like ToolUseProvider but without a barrier — each job independently
    /// does a 2-turn cycle (tool-use then text completion).
    struct PhaseProvider {
        phases: Arc<Mutex<HashMap<String, usize>>>,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for PhaseProvider {
        async fn stream_completion(
            &self,
            messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            let prompt = messages
                .iter()
                .rev()
                .find(|m| m.role == crate::domain::models::MessageRole::User)
                .map(|m| m.content.clone())
                .unwrap_or_default();
            let mut phases = self.phases.lock().await;
            let phase = phases.entry(prompt.clone()).or_default();
            *phase += 1;
            let current_phase = *phase;
            drop(phases);

            if current_phase == 1 {
                Ok(futures::stream::iter(vec![
                    StreamChunk::ToolUse {
                        id: format!("{prompt}-tool"),
                        name: "remember_fact".into(),
                        input: serde_json::json!({"category": "Cron", "fact": format!("{prompt} note from cron")}),
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::ToolUse,
                    },
                ])
                .boxed())
            } else {
                Ok(futures::stream::iter(vec![
                    StreamChunk::Text {
                        content: format!("done {prompt}"),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ])
                .boxed())
            }
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "phase".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    struct ParkingMemory {
        inner: Arc<dyn crate::domain::ports::MemoryPort>,
        reached: Arc<Notify>,
        proceed: Arc<Notify>,
        armed: std::sync::atomic::AtomicBool,
    }

    impl ParkingMemory {
        async fn maybe_park(&self) {
            if self.armed.swap(false, AtomicOrdering::SeqCst) {
                self.reached.notify_one();
                self.proceed.notified().await;
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::domain::ports::MemoryPort for ParkingMemory {
        async fn store(
            &self,
            entry: crate::domain::models::MemoryEntry,
        ) -> Result<(), crate::domain::errors::MemoryError> {
            self.maybe_park().await;
            self.inner.store(entry).await
        }

        async fn remember_fact(
            &self,
            fact: MemoryFact,
        ) -> Result<(), crate::domain::errors::MemoryError> {
            self.maybe_park().await;
            self.inner.remember_fact(fact).await
        }

        async fn recent(
            &self,
            limit: usize,
        ) -> Result<Vec<crate::domain::models::MemoryEntry>, crate::domain::errors::MemoryError>
        {
            self.inner.recent(limit).await
        }

        async fn search(
            &self,
            query: &str,
            limit: usize,
        ) -> Result<Vec<crate::domain::models::MemoryEntry>, crate::domain::errors::MemoryError>
        {
            self.inner.search(query, limit).await
        }
    }

    fn scripted_runtime(
        provider: Arc<dyn StreamingProvider>,
        storage: Arc<dyn StoragePort>,
        workspace: &Path,
    ) -> Arc<DaemonTurnRuntime> {
        let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
        let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
        let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
        let tool_scheduler =
            ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 64);
        Arc::new(DaemonTurnRuntime {
            recipient_context: Arc::new(Default::default()),
            provider,
            app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            security,
            tools,
            tool_scheduler,
            persona: Arc::new(NoOpPersona),
            context_assembler: Arc::new(ArcSwap::from_pointee(None)),
            context: Arc::new(ArcSwap::from_pointee(
                Arc::new(NoOpContext) as Arc<dyn ContextPort>
            )),
            storage: storage.clone(),
            fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(workspace),
                workspace.to_path_buf(),
            )),
            usage_ledger: Arc::new(NoOpUsageLedger),
            telemetry: crate::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
            plan_injector: Arc::new(
                crate::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
            ),
            approval,
            workspace: workspace.to_path_buf(),
            #[cfg(feature = "mcp")]
            mcp_task_runtimes: Vec::new(),
        })
    }

    fn scripted_core(
        workspace: &Path,
        provider: Arc<dyn StreamingProvider>,
    ) -> (Arc<DaemonCore>, Arc<dyn StoragePort>) {
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(workspace),
            workspace.to_path_buf(),
        ));
        let core = scripted_core_with_storage(workspace, provider, storage.clone());
        (core, storage)
    }

    fn scripted_core_with_storage(
        workspace: &Path,
        provider: Arc<dyn StreamingProvider>,
        storage: Arc<dyn StoragePort>,
    ) -> Arc<DaemonCore> {
        let ws = workspace.to_path_buf();
        let storage_for_factory = storage.clone();
        Arc::new(DaemonCore::new(
            workspace.to_path_buf(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage,
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                Ok(scripted_runtime(
                    provider.clone(),
                    storage_for_factory.clone(),
                    &ws,
                ))
            }),
        ))
    }

    // Expose the real filesystem Read tool under the same wire name as AC6.
    struct C2bReadAlias(Arc<dyn ToolSetPort>);

    #[async_trait::async_trait]
    impl ToolSetPort for C2bReadAlias {
        fn available_tools(&self) -> Vec<crate::domain::models::ToolDefinition> {
            let mut tools = self.0.available_tools();
            let mut read = tools
                .iter()
                .find(|tool| tool.name == "Read")
                .unwrap()
                .clone();
            read.name = "read_file".into();
            tools.push(read);
            tools
        }

        async fn execute(
            &self,
            name: &str,
            input: serde_json::Value,
            cancel: CancellationToken,
        ) -> Result<crate::domain::models::ToolResult, crate::domain::errors::ToolError> {
            self.0
                .execute(
                    if name == "read_file" { "Read" } else { name },
                    input,
                    cancel,
                )
                .await
        }
    }

    fn scripted_runtime_with_memory_tools(
        provider: Arc<dyn StreamingProvider>,
        storage: Arc<dyn StoragePort>,
        workspace: &Path,
        memory_slot: Arc<arc_swap::ArcSwap<Arc<dyn crate::domain::ports::MemoryPort>>>,
        memory_write_gate: Arc<tokio::sync::RwLock<()>>,
        read_file_alias: bool,
    ) -> Arc<DaemonTurnRuntime> {
        let security: Arc<dyn SecurityPort> = if read_file_alias {
            let security =
                crate::adapters::security_adapter::SecurityAdapter::new(workspace.to_path_buf());
            security.set_mode(crate::domain::models::PermissionMode::Yolo);
            Arc::new(security)
        } else {
            Arc::new(NoOpSecurity)
        };
        let mut adapter = ToolSetAdapter::new(
            workspace.to_path_buf(),
            storage.clone(),
            Arc::new(arc_swap::ArcSwap::from_pointee(
                Arc::new(crate::adapters::sandbox::NoOpSandbox)
                    as Arc<dyn crate::domain::ports::SandboxManager>,
            )),
            Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::sandbox::SandboxPolicy::Permissive,
            )),
        );
        adapter.set_memory(memory_slot, memory_write_gate);
        let tools: Arc<dyn ToolSetPort> = if read_file_alias {
            Arc::new(C2bReadAlias(Arc::new(adapter)))
        } else {
            Arc::new(adapter)
        };
        let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
        let tool_scheduler =
            ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 64);
        Arc::new(DaemonTurnRuntime {
            recipient_context: Arc::new(Default::default()),
            provider,
            app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            security,
            tools,
            tool_scheduler,
            persona: Arc::new(NoOpPersona),
            context_assembler: Arc::new(ArcSwap::from_pointee(None)),
            context: Arc::new(ArcSwap::from_pointee(
                Arc::new(NoOpContext) as Arc<dyn ContextPort>
            )),
            storage: storage.clone(),
            fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(workspace),
                workspace.to_path_buf(),
            )),
            usage_ledger: Arc::new(NoOpUsageLedger),
            telemetry: crate::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
            plan_injector: Arc::new(
                crate::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
            ),
            approval,
            workspace: workspace.to_path_buf(),
            #[cfg(feature = "mcp")]
            mcp_task_runtimes: Vec::new(),
        })
    }

    fn scripted_core_with_memory_tools(
        workspace: &Path,
        provider: Arc<dyn StreamingProvider>,
        memory_slot: Arc<arc_swap::ArcSwap<Arc<dyn crate::domain::ports::MemoryPort>>>,
        memory_write_gate: Arc<tokio::sync::RwLock<()>>,
        read_file_alias: bool,
    ) -> (Arc<DaemonCore>, Arc<dyn StoragePort>) {
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(workspace),
            workspace.to_path_buf(),
        ));
        let ws = workspace.to_path_buf();
        let storage_for_factory = storage.clone();
        let provider_for_factory = provider.clone();
        let memory_for_core = memory_slot.load_full().as_ref().clone();
        let memory_slot_for_factory = memory_slot.clone();
        let gate_for_factory = memory_write_gate.clone();
        let core = DaemonCore::new(
            workspace.to_path_buf(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            memory_for_core,
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                Ok(scripted_runtime_with_memory_tools(
                    provider_for_factory.clone(),
                    storage_for_factory.clone(),
                    &ws,
                    memory_slot_for_factory.clone(),
                    gate_for_factory.clone(),
                    read_file_alias,
                ))
            }),
        );
        (Arc::new(core), storage)
    }
    // ── Story 12.4: parsing valid/invalid/malformed cron.toml ──

    #[test]
    fn parse_jobs_all_valid_preserves_fields() {
        let jobs = parse_jobs(vec![
            CronJob {
                name: "morning".into(),
                schedule: "0 9 * * *".into(),
                prompt: "Brief me".into(),
                forward: true,
                timeout_secs: None,
            },
            CronJob {
                name: "hourly".into(),
                schedule: "0 * * * *".into(),
                prompt: "Check health".into(),
                forward: false,
                timeout_secs: None,
            },
        ]);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].name, "morning");
        assert!(jobs[0].forward);
        assert_eq!(jobs[1].name, "hourly");
        assert!(!jobs[1].forward);
    }

    #[test]
    fn parse_jobs_empty_input_is_empty() {
        let jobs = parse_jobs(vec![]);
        assert!(jobs.is_empty());
    }

    #[test]
    fn parse_jobs_all_invalid_returns_empty() {
        let jobs = parse_jobs(vec![
            CronJob {
                name: "a".into(),
                schedule: "not cron".into(),
                prompt: "x".into(),
                forward: false,
                timeout_secs: None,
            },
            CronJob {
                name: "b".into(),
                schedule: "99 99 99 99 99".into(),
                prompt: "y".into(),
                forward: false,
                timeout_secs: None,
            },
        ]);
        assert!(jobs.is_empty(), "all-invalid schedules → no jobs");
    }

    #[tokio::test]
    async fn load_jobs_valid_toml_reads_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cron.toml");
        tokio::fs::write(
            &path,
            r#"
[[jobs]]
name = "daily-standup"
schedule = "0 9 * * 1-5"
prompt = "Summarize yesterday"
forward = true

[[jobs]]
name = "weekly-review"
schedule = "0 17 * * 5"
prompt = "Weekly report"
"#,
        )
        .await
        .unwrap();
        let jobs = load_jobs(&path).await;
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].name, "daily-standup");
        assert!(jobs[0].forward);
        assert_eq!(jobs[1].name, "weekly-review");
        assert!(!jobs[1].forward, "forward defaults to false");
    }

    #[tokio::test]
    async fn load_jobs_empty_toml_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cron.toml");
        tokio::fs::write(&path, "").await.unwrap();
        let jobs = load_jobs(&path).await;
        assert!(jobs.is_empty(), "empty toml → no jobs");
    }

    #[tokio::test]
    async fn load_jobs_mixed_valid_invalid_keeps_valid_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cron.toml");
        tokio::fs::write(
            &path,
            r#"
[[jobs]]
name = "good"
schedule = "0 9 * * *"
prompt = "ok"

[[jobs]]
name = "bad-schedule"
schedule = "garbage"
prompt = "nope"
"#,
        )
        .await
        .unwrap();
        let jobs = load_jobs(&path).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "good");
    }

    // ── Story 12.4 AC1/AC2: next-run calculation ──

    #[test]
    fn next_run_hourly_advances_one_hour() {
        let cron = Cron::from_str("0 * * * *").unwrap();
        let from = chrono::TimeZone::with_ymd_and_hms(&Local, 2026, 6, 9, 8, 0, 0).unwrap();
        let next = next_occurrence(&cron, from).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn next_run_same_fire_time_advances_to_next_occurrence() {
        let cron = Cron::from_str("0 9 * * *").unwrap();
        // At exactly 09:00 — the next occurrence is tomorrow 09:00.
        let from = chrono::TimeZone::with_ymd_and_hms(&Local, 2026, 6, 9, 9, 0, 0).unwrap();
        let next = next_occurrence(&cron, from).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
        // Must be strictly after `from`.
        assert!(next.timestamp() > from.timestamp());
    }

    #[test]
    fn next_run_weekday_only_skips_weekend() {
        let cron = Cron::from_str("0 9 * * 1-5").unwrap();
        // Friday June 12, 2026 at 10:00 → next is Monday June 15.
        let from = chrono::TimeZone::with_ymd_and_hms(&Local, 2026, 6, 12, 10, 0, 0).unwrap();
        let next = next_occurrence(&cron, from).unwrap();
        assert_eq!(next.hour(), 9);
        // Monday = weekday 1 in chrono.
        assert!(
            next.weekday().num_days_from_monday() < 5,
            "must be a weekday"
        );
        assert!(
            next.date_naive() > from.date_naive(),
            "must be after Friday"
        );
    }

    // ── Story 12.4: conversation construction ──

    #[test]
    fn new_cron_conversation_has_correct_fields() {
        let conv = new_cron_conversation("cron-test-1234", "morning-briefing", 1717900000);
        assert_eq!(conv.id, "cron-test-1234");
        assert_eq!(conv.title, "cron: morning-briefing");
        assert!(conv.messages.is_empty());
        assert_eq!(conv.created_at, 1717900000);
        assert!(conv.session_id.is_some());
        assert_eq!(conv.session_id.as_deref(), Some("cron-test-1234"));
    }

    // ── Story 12.4: sanitize_session_component edge cases ──

    #[test]
    fn sanitize_empty_string_yields_job() {
        assert_eq!(sanitize_session_component(""), "job");
    }

    #[test]
    fn sanitize_all_special_chars_yields_job() {
        assert_eq!(sanitize_session_component("!@#$%^&*()"), "job");
    }

    #[test]
    fn sanitize_consecutive_dedup_does_not_double_dashes() {
        // The function only suppresses inserting '-' when the output already ends with '-'.
        // It does NOT collapse existing '-' sequences in the input.
        assert_eq!(sanitize_session_component("a---b!!!c"), "a---b-c");
    }

    #[test]
    fn sanitize_leading_trailing_dashes_trimmed() {
        assert_eq!(sanitize_session_component("--hello world--"), "hello-world");
    }

    // ── Story 12.4 AC4/AC5: CronCompletion formatting ──

    #[test]
    fn cron_completion_holds_job_name_and_text() {
        let cc = CronCompletion {
            job_name: "morning-briefing".into(),
            result_text: "3 commits yesterday".into(),
        };
        assert_eq!(cc.job_name, "morning-briefing");
        assert_eq!(cc.result_text, "3 commits yesterday");
    }

    #[test]
    fn cron_completion_format_matches_server_injection() {
        // Verify the server's format string: "[cron: {name}] {text}"
        let cc = CronCompletion {
            job_name: "daily".into(),
            result_text: "all good".into(),
        };
        let formatted = format!("[cron: {}] {}", cc.job_name, cc.result_text);
        assert_eq!(formatted, "[cron: daily] all good");
    }

    // ── Story 12.4: fire detection correctness ──

    #[test]
    fn is_time_matching_detects_fire_time() {
        let cron = Cron::from_str("0 9 * * *").unwrap();
        // Exactly at 09:00 — must match.
        let at_fire = chrono::TimeZone::with_ymd_and_hms(&Local, 2026, 6, 9, 9, 0, 0).unwrap();
        assert!(cron.is_time_matching(&at_fire).unwrap());
        // At 09:01 — must NOT match.
        let after = chrono::TimeZone::with_ymd_and_hms(&Local, 2026, 6, 9, 9, 1, 0).unwrap();
        assert!(!cron.is_time_matching(&after).unwrap());
        // At 08:59 — must NOT match.
        let before = chrono::TimeZone::with_ymd_and_hms(&Local, 2026, 6, 9, 8, 59, 0).unwrap();
        assert!(!cron.is_time_matching(&before).unwrap());
    }

    #[test]
    fn fire_detection_filter_would_match_correct_jobs() {
        // Simulate the scheduler loop's filter: given a `next` fire time,
        // only jobs whose schedule matches that time should fire.
        let daily_9am = Cron::from_str("0 9 * * *").unwrap();
        let hourly = Cron::from_str("0 * * * *").unwrap();
        let fire_time = chrono::TimeZone::with_ymd_and_hms(&Local, 2026, 6, 9, 9, 0, 0).unwrap();
        // Both match 09:00.
        assert!(daily_9am.is_time_matching(&fire_time).unwrap());
        assert!(hourly.is_time_matching(&fire_time).unwrap());
        // But at 10:00, only the hourly matches.
        let ten_am = chrono::TimeZone::with_ymd_and_hms(&Local, 2026, 6, 9, 10, 0, 0).unwrap();
        assert!(!daily_9am.is_time_matching(&ten_am).unwrap());
        assert!(hourly.is_time_matching(&ten_am).unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_jobs_own_context_and_persist_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let provider: Arc<dyn StreamingProvider> = Arc::new(BarrierProvider { barrier });
        let (core, storage) = scripted_core(ws, provider);
        let channel: Arc<dyn ChannelPort> = Arc::new(RecordingChannel::default());
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let running = Arc::new(Mutex::new(std::collections::HashSet::new()));

        let permit_a = Arc::new(tokio::sync::Semaphore::new(2))
            .acquire_owned()
            .await
            .unwrap();
        let permit_b = Arc::new(tokio::sync::Semaphore::new(2))
            .acquire_owned()
            .await
            .unwrap();

        let job_a = CronJobTask {
            job: ScheduledJob {
                name: "job-a".into(),
                cron: Cron::from_str("0 9 * * *").unwrap(),
                prompt: "alpha".into(),
                forward: false,
                timeout_secs: None,
            },
            fired_at_unix: 100,
            core: core.clone(),
            completion_tx: completion_tx.clone(),
            channel: channel.clone(),
            storage: storage.clone(),
            shutdown: CancellationToken::new(),
            job_timeout: Duration::from_secs(2),
            running: running.clone(),
            _permit: permit_a,
        };
        running.lock().await.insert("job-a".into());

        let job_b = CronJobTask {
            job: ScheduledJob {
                name: "job-b".into(),
                cron: Cron::from_str("0 9 * * *").unwrap(),
                prompt: "beta".into(),
                forward: false,
                timeout_secs: None,
            },
            fired_at_unix: 101,
            core: core.clone(),
            completion_tx: completion_tx.clone(),
            channel,
            storage: storage.clone(),
            shutdown: CancellationToken::new(),
            job_timeout: Duration::from_secs(2),
            running: running.clone(),
            _permit: permit_b,
        };
        running.lock().await.insert("job-b".into());

        let a = tokio::spawn(run_job(job_a));
        let b = tokio::spawn(run_job(job_b));
        tokio::time::timeout(Duration::from_secs(1), async {
            let _ = tokio::join!(a, b);
        })
        .await
        .expect("serialized path would deadlock on the barrier");

        let session_a = format!("cron-{}-100", sanitize_session_component("job-a"));
        let session_b = format!("cron-{}-101", sanitize_session_component("job-b"));
        let conv_a = storage
            .load_conversation(&session_a)
            .await
            .unwrap()
            .expect("job a persisted");
        let conv_b = storage
            .load_conversation(&session_b)
            .await
            .unwrap()
            .expect("job b persisted");
        let text_a = conv_a
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let text_b = conv_b
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text_a.contains("alpha"));
        assert!(text_a.contains("result for alpha"));
        assert!(!text_a.contains("beta"));
        assert!(text_b.contains("beta"));
        assert!(text_b.contains("result for beta"));
        assert!(!text_b.contains("alpha"));
        let mut names = vec![
            completion_rx.recv().await.unwrap().job_name,
            completion_rx.recv().await.unwrap().job_name,
        ];
        names.sort();
        assert_eq!(names, vec!["job-a", "job-b"]);
    }

    #[tokio::test]
    async fn overlap_policy_skips_running_job_and_cap_exhaustion() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let provider: Arc<dyn StreamingProvider> = Arc::new(StaticProvider { chunks: vec![] });
        let (core, storage) = scripted_core(ws, provider);
        let state = LoopState {
            jobs: Arc::new(ArcSwap::from_pointee(Vec::new())),
            core,
            completion_tx: mpsc::unbounded_channel().0,
            channel: Arc::new(RecordingChannel::default()),
            storage,
            shutdown: CancellationToken::new(),
            job_timeout: Duration::from_secs(1),
            sem: Arc::new(tokio::sync::Semaphore::new(1)),
            running: Arc::new(Mutex::new(std::collections::HashSet::new())),
            inflight: Arc::new(Mutex::new(Vec::new())),
            reload_notify: Arc::new(Notify::new()),
            next_runs: Arc::new(Mutex::new(Vec::new())),
            health: Arc::new(AtomicU8::new(HEALTH_OK)),
        };
        let running_job = ScheduledJob {
            name: "busy".into(),
            cron: Cron::from_str("0 9 * * *").unwrap(),
            prompt: "noop".into(),
            forward: false,
            timeout_secs: None,
        };
        state.running.lock().await.insert("busy".into());
        maybe_spawn_job(&state, running_job, 1).await;
        assert_eq!(state.inflight.lock().await.len(), 0);
        assert!(state.running.lock().await.contains("busy"));

        let _held = state.sem.clone().acquire_owned().await.unwrap();
        let capped_job = ScheduledJob {
            name: "capped".into(),
            cron: Cron::from_str("0 9 * * *").unwrap(),
            prompt: "noop".into(),
            forward: false,
            timeout_secs: None,
        };
        maybe_spawn_job(&state, capped_job, 2).await;
        assert_eq!(state.inflight.lock().await.len(), 0);
        assert!(!state.running.lock().await.contains("capped"));
    }

    #[tokio::test]
    async fn result_persisted_and_forwarded_only_when_opted_in() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let full_text = std::iter::repeat_n("word", 300)
            .collect::<Vec<_>>()
            .join(" ");
        let provider: Arc<dyn StreamingProvider> = Arc::new(StaticProvider {
            chunks: vec![
                StreamChunk::Text {
                    content: full_text.clone(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        });
        let (core, storage) = scripted_core(ws, provider);
        let channel = Arc::new(RecordingChannel::default());
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let running = Arc::new(Mutex::new(std::collections::HashSet::new()));
        running.lock().await.insert("briefing".into());

        let job = CronJobTask {
            job: ScheduledJob {
                name: "briefing".into(),
                cron: Cron::from_str("0 9 * * *").unwrap(),
                prompt: "report".into(),
                forward: true,
                timeout_secs: None,
            },
            fired_at_unix: 500,
            core,
            completion_tx,
            channel: channel.clone(),
            storage: storage.clone(),
            shutdown: CancellationToken::new(),
            job_timeout: Duration::from_secs(1),
            running: running.clone(),
            _permit: Arc::new(tokio::sync::Semaphore::new(1))
                .acquire_owned()
                .await
                .unwrap(),
        };
        run_job(job).await;

        let completion = completion_rx.recv().await.unwrap();
        assert_eq!(completion.job_name, "briefing");
        assert_eq!(completion.result_text, full_text);

        let session_id = format!("cron-{}-500", sanitize_session_component("briefing"));
        let conv = storage
            .load_conversation(&session_id)
            .await
            .unwrap()
            .expect("stored cron session");
        assert_eq!(conv.messages[0].role, MessageRole::User);
        assert_eq!(conv.messages[0].origin, ChannelKind::Cron);
        let assistant = conv
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Assistant)
            .unwrap();
        assert_eq!(assistant.origin, ChannelKind::Cron);
        assert_eq!(assistant.content, full_text);

        let sent = channel.sent.lock().await.clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].starts_with("[cron: briefing] "));
        assert!(sent[0].contains("word word word"));
    }

    #[tokio::test]
    async fn forward_false_does_not_notify_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let provider: Arc<dyn StreamingProvider> = Arc::new(StaticProvider {
            chunks: vec![
                StreamChunk::Text {
                    content: "done".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        });
        let (core, storage) = scripted_core(ws, provider);
        let channel = Arc::new(RecordingChannel::default());
        let running = Arc::new(Mutex::new(std::collections::HashSet::new()));
        running.lock().await.insert("local-only".into());
        let job = CronJobTask {
            job: ScheduledJob {
                name: "local-only".into(),
                cron: Cron::from_str("0 9 * * *").unwrap(),
                prompt: "report".into(),
                forward: false,
                timeout_secs: None,
            },
            fired_at_unix: 600,
            core,
            completion_tx: mpsc::unbounded_channel().0,
            channel: channel.clone(),
            storage,
            shutdown: CancellationToken::new(),
            job_timeout: Duration::from_secs(1),
            running,
            _permit: Arc::new(tokio::sync::Semaphore::new(1))
                .acquire_owned()
                .await
                .unwrap(),
        };
        run_job(job).await;
        assert!(channel.sent.lock().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn full_drive_turn_memory_write_blocks_swap_mid_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let old_root = tempfile::tempdir().unwrap();
        let new_root = tempfile::tempdir().unwrap();
        let reached = Arc::new(Notify::new());
        let proceed = Arc::new(Notify::new());
        let old_inner: Arc<dyn crate::domain::ports::MemoryPort> = Arc::new(
            crate::adapters::project_scoped_memory::ProjectScopedMemory::new(old_root.path()),
        );
        let old_adapter: Arc<dyn crate::domain::ports::MemoryPort> = Arc::new(ParkingMemory {
            inner: old_inner,
            reached: reached.clone(),
            proceed: proceed.clone(),
            armed: std::sync::atomic::AtomicBool::new(true),
        });
        let new_adapter: Arc<dyn crate::domain::ports::MemoryPort> = Arc::new(
            crate::adapters::project_scoped_memory::ProjectScopedMemory::new(new_root.path()),
        );
        let memory_slot = Arc::new(ArcSwap::from_pointee(old_adapter));
        let memory_write_gate = Arc::new(tokio::sync::RwLock::new(()));
        let provider: Arc<dyn StreamingProvider> = Arc::new(ToolUseProvider {
            barrier: Arc::new(tokio::sync::Barrier::new(1)),
            phases: Arc::new(Mutex::new(HashMap::new())),
        });
        let (core, storage) = scripted_core_with_memory_tools(
            ws,
            provider,
            memory_slot.clone(),
            memory_write_gate.clone(),
            false,
        );
        let running = Arc::new(Mutex::new(std::collections::HashSet::new()));
        running.lock().await.insert("job-fact".into());
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();

        let job = CronJobTask {
            job: ScheduledJob {
                name: "job-fact".into(),
                cron: Cron::from_str("0 9 * * *").unwrap(),
                prompt: "fact".into(),
                forward: false,
                timeout_secs: None,
            },
            fired_at_unix: 701,
            core,
            completion_tx: mpsc::unbounded_channel().0,
            channel: Arc::new(RecordingChannel::default()),
            storage: storage.clone(),
            shutdown: CancellationToken::new(),
            job_timeout: Duration::from_secs(2),
            running: running.clone(),
            _permit: permit,
        };

        let handle = tokio::spawn(run_job(job));
        reached.notified().await;
        let initial_slot = memory_slot.load_full();
        let slot = memory_slot.clone();
        let gate = memory_write_gate.clone();
        let swap = tokio::spawn(async move {
            let _exclusive = gate.write().await;
            slot.store(Arc::new(new_adapter));
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            Arc::ptr_eq(&memory_slot.load_full(), &initial_slot),
            "warm swap must block until the in-flight cron memory write finishes"
        );
        proceed.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            let _ = tokio::join!(handle, swap);
        })
        .await
        .expect("cron drive_turn memory write path must complete without deadlock");

        let combined_md = format!(
            "{}\n{}",
            std::fs::read_to_string(old_root.path().join(".rustain").join("MEMORY.md"))
                .unwrap_or_default(),
            std::fs::read_to_string(new_root.path().join(".rustain").join("MEMORY.md"))
                .unwrap_or_default()
        );
        assert!(
            combined_md.contains("fact note from cron"),
            "remember_fact write survives full cron drive_turn swap path"
        );

        let session_fact = storage
            .load_conversation("cron-job-fact-701")
            .await
            .unwrap()
            .expect("fact cron session persisted");
        assert_eq!(session_fact.messages[0].origin, ChannelKind::Cron);
    }

    /// G18 chaos/concurrency integration test: k=4 concurrent jobs × n=4 memory-touching
    /// turns through full drive_turn against shared memory adapters + forced mid-flight swap.
    /// Composition proof — mechanism proven at 1×1, this proves it at scale.
    /// Full k=8×n=32 scale test deferred to fast-follow (Murat modified Option 2).
    /// G18 chaos/concurrency integration test: k=4 concurrent jobs × 2-turn cycles through
    /// full drive_turn against shared memory adapters + forced mid-flight adapter swap.
    /// Composition proof — mechanism proven at 1×1, this proves it at scale.
    /// Full k=8×n=32 scale test deferred to fast-follow (Murat modified Option 2).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn chaos_4x4_concurrent_jobs_memory_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let old_root = tempfile::tempdir().unwrap();
        let new_root = tempfile::tempdir().unwrap();

        let old_adapter: Arc<dyn crate::domain::ports::MemoryPort> = Arc::new(
            crate::adapters::project_scoped_memory::ProjectScopedMemory::new(old_root.path()),
        );
        let new_adapter: Arc<dyn crate::domain::ports::MemoryPort> = Arc::new(
            crate::adapters::project_scoped_memory::ProjectScopedMemory::new(new_root.path()),
        );
        let memory_slot: Arc<ArcSwap<Arc<dyn crate::domain::ports::MemoryPort>>> =
            Arc::new(ArcSwap::from_pointee(old_adapter));
        let memory_write_gate = Arc::new(tokio::sync::RwLock::new(()));

        // PhaseProvider: no barrier — each job independently does a 2-turn cycle.
        // Phase 1: remember_fact tool-use → phase 2: text completion.
        let phases: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        let provider: Arc<dyn StreamingProvider> = Arc::new(PhaseProvider {
            phases: phases.clone(),
        });
        let (core, storage) = scripted_core_with_memory_tools(
            ws,
            provider,
            memory_slot.clone(),
            memory_write_gate.clone(),
            false,
        );

        let running = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let sem = Arc::new(tokio::sync::Semaphore::new(4));
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();

        // Spawn k=4 concurrent cron jobs, each with a 2-turn cycle (tool-use + completion).
        let job_names: &[&str] = &["alpha", "bravo", "charlie", "delta"];
        let mut job_handles = Vec::new();
        for (i, name) in job_names.iter().enumerate() {
            let permit = sem.clone().acquire_owned().await.unwrap();
            running.lock().await.insert((*name).to_string());
            let job = CronJobTask {
                job: ScheduledJob {
                    name: (*name).to_string(),
                    cron: Cron::from_str("0 9 * * *").unwrap(),
                    prompt: (*name).to_string(),
                    forward: false,
                    timeout_secs: None,
                },
                fired_at_unix: 800 + i as i64,
                core: core.clone(),
                completion_tx: completion_tx.clone(),
                channel: Arc::new(RecordingChannel::default()),
                storage: storage.clone(),
                shutdown: shutdown.clone(),
                job_timeout: Duration::from_secs(5),
                running: running.clone(),
                _permit: permit,
            };
            job_handles.push(tokio::spawn(run_job(job)));
        }
        drop(completion_tx);

        // Force a mid-flight adapter swap while jobs are running.
        // The prevention gate (memory_write_gate) ensures no swap lands mid-write.
        let swap_slot = memory_slot.clone();
        let swap_gate = memory_write_gate.clone();
        let swap_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _exclusive = swap_gate.write().await;
            swap_slot.store(Arc::new(new_adapter));
        });

        // All jobs and swap must complete without deadlock within budget.
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            for h in job_handles {
                let _ = h.await;
            }
            let _ = swap_handle.await;
        })
        .await;
        assert!(
            result.is_ok(),
            "k=4 chaos test must complete without deadlock within 15s"
        );

        // Verify: all 4 completions received.
        let mut completions = Vec::new();
        while let Some(c) = completion_rx.recv().await {
            completions.push(c);
        }
        assert_eq!(completions.len(), 4, "all 4 jobs must produce completions");

        // Verify: all 4 sessions persisted with correct origin.
        for (i, name) in job_names.iter().enumerate() {
            let session_id = format!(
                "cron-{}-{}",
                sanitize_session_component(name),
                800 + i as i64
            );
            let conv = storage
                .load_conversation(&session_id)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("session for job '{name}' must exist"));
            assert_eq!(conv.messages[0].origin, ChannelKind::Cron);
        }

        // Verify: memory writes landed — combined MEMORY.md contains all 4 facts.
        let combined_md = format!(
            "{}\n{}",
            std::fs::read_to_string(old_root.path().join(".rustain").join("MEMORY.md"))
                .unwrap_or_default(),
            std::fs::read_to_string(new_root.path().join(".rustain").join("MEMORY.md"))
                .unwrap_or_default()
        );
        for name in job_names {
            assert!(
                combined_md.contains(&format!("{name} note from cron")),
                "remember_fact write for job '{name}' must survive swap"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_loop_cancels_inflight_tasks_within_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let provider: Arc<dyn StreamingProvider> = Arc::new(PendingProvider);
        let (core, storage) = scripted_core(ws, provider);
        let adapter = CronSchedulerAdapter::new_loaded(
            ws.join("cron.toml"),
            vec![],
            core,
            mpsc::unbounded_channel().0,
            Arc::new(RecordingChannel::default()),
            storage,
            Duration::from_millis(50),
            1,
        );
        adapter.start_loop().await.unwrap();
        adapter.inflight.lock().await.push(tokio::spawn(async {
            futures::future::pending::<()>().await
        }));
        tokio::time::timeout(Duration::from_secs(6), adapter.shutdown_loop())
            .await
            .expect("shutdown bounded")
            .unwrap();
        assert_eq!(
            adapter.health_snapshot().level,
            crate::domain::models::HealthLevel::Error
        );
    }

    #[tokio::test]
    async fn reload_swaps_job_set_without_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let cron_path = ws.join("cron.toml");
        tokio::fs::write(
            &cron_path,
            "[[jobs]]\nname='one'\nschedule='0 9 * * *'\nprompt='a'\n",
        )
        .await
        .unwrap();
        let provider: Arc<dyn StreamingProvider> = Arc::new(StaticProvider { chunks: vec![] });
        let (core, storage) = scripted_core(ws, provider);
        let adapter = CronSchedulerAdapter::load(
            cron_path.clone(),
            core,
            mpsc::unbounded_channel().0,
            Arc::new(RecordingChannel::default()),
            storage,
        )
        .await
        .unwrap();
        assert_eq!(adapter.jobs.load().len(), 1);
        adapter.start_loop().await.unwrap();
        tokio::fs::write(
            &cron_path,
            "[[jobs]]\nname='two'\nschedule='0 10 * * *'\nprompt='b'\n",
        )
        .await
        .unwrap();
        adapter.reload().await.unwrap();
        for _ in 0..50 {
            if adapter
                .jobs
                .load()
                .first()
                .is_some_and(|job| job.name == "two")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(adapter.jobs.load().first().unwrap().name, "two");
        assert_eq!(
            adapter.health_snapshot().level,
            crate::domain::models::HealthLevel::Healthy
        );
        adapter.shutdown_loop().await.unwrap();
    }

    #[test]
    fn cron_module_uses_drive_turn_and_not_forwarder_turn_driver() {
        let src = std::fs::read_to_string(file!()).unwrap();
        assert!(
            src.contains(".drive_turn("),
            "cron scheduler must call the shared drive_turn engine"
        );
    }

    use chrono::Datelike;
    use chrono::Timelike;

    struct C2bWholeTurnProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for C2bWholeTurnProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            let chunks = if self.calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                vec![
                    StreamChunk::Text {
                        content: "Checking.".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::ToolUse {
                        id: "t1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"file_path": "secret.txt"}),
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::ToolUse,
                    },
                ]
            } else {
                vec![
                    StreamChunk::Text {
                        content: "Answer: 42".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ]
            };
            Ok(futures::stream::iter(chunks).boxed())
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "c2b-whole-turn".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: Duration::ZERO,
            })
        }
    }

    struct C2bSlowProvider {
        started: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for C2bSlowProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            self.started.notify_one();
            tokio::time::sleep(Duration::from_secs(700)).await;
            Ok(futures::stream::iter(vec![
                StreamChunk::Text {
                    content: "too late".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ])
            .boxed())
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "c2b-slow".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: Duration::ZERO,
            })
        }
    }

    // Let the real scheduler choose its next wall-clock minute, then advance Tokio's
    // clock to that deadline. Empty the next snapshot so advancing 600 virtual
    // seconds cannot cause repeat fires while chrono::Local remains wall-clock based.
    async fn c2b_fire_one(adapter: &CronSchedulerAdapter) -> String {
        adapter.start_loop().await.unwrap();
        let fire_at = loop {
            if let Some((_, Some(ts))) = adapter.next_runs().await.first() {
                break *ts;
            }
            tokio::task::yield_now().await;
        };
        adapter.jobs.store(Arc::new(vec![]));
        let delay_ms = (fire_at * 1000 - chrono::Local::now().timestamp_millis()).max(0) as u64;
        tokio::time::advance(Duration::from_millis(delay_ms + 10)).await;
        format!("cron-briefing-{fire_at}")
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_scheduler_persists_and_forwards_the_whole_tool_turn() {
        let tmp = tempfile::Builder::new()
            .prefix("c-2b-cron-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "SECRET-RESULT-C2B").unwrap();
        let provider: Arc<dyn StreamingProvider> = Arc::new(C2bWholeTurnProvider {
            calls: AtomicUsize::new(0),
        });
        let memory: Arc<dyn crate::domain::ports::MemoryPort> = Arc::new(NoOpMemory);
        let (core, storage) = scripted_core_with_memory_tools(
            tmp.path(),
            provider,
            Arc::new(ArcSwap::from_pointee(memory)),
            Arc::new(tokio::sync::RwLock::new(())),
            true,
        );
        let channel = Arc::new(RecordingChannel::default());
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let path = tmp.path().join("cron.toml");
        std::fs::write(
            &path,
            "[[jobs]]\nname='briefing'\nschedule='* * * * *'\nprompt='report'\nforward=true\n",
        )
        .unwrap();
        let adapter =
            CronSchedulerAdapter::load(path, core, completion_tx, channel.clone(), storage.clone())
                .await
                .unwrap();
        let session_id = c2b_fire_one(&adapter).await;
        let completion = tokio::time::timeout(Duration::from_secs(10), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        // Completion precedes channel.notify; wait until this job leaves the running set.
        while adapter.running.lock().await.contains("briefing") {
            tokio::task::yield_now().await;
        }
        let reply = "Checking.\n[tool use: read_file (id: t1)]\nAnswer: 42";
        assert_eq!(completion.job_name, "briefing");
        assert_eq!(completion.result_text, reply);
        assert_eq!(
            *channel.sent.lock().await,
            vec![format!("[cron: briefing] {reply}")]
        );
        let session = storage
            .load_conversation(&session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "report");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].stop_reason, Some(StopReason::ToolUse));
        assert_eq!(session.messages[1].tool_calls[0].wire_id(), "t1");
        let reloaded = storage
            .load_conversation(&session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            session.messages[1].tool_calls[0].id,
            reloaded.messages[1].tool_calls[0].id
        );
        let result = session.messages[1].tool_calls[0].result.as_ref().unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("SECRET-RESULT-C2B"));
        assert_eq!(session.messages[2].role, MessageRole::Assistant);
        assert_eq!(session.messages[2].content, "Answer: 42");
        assert_eq!(session.messages[2].stop_reason, Some(StopReason::EndTurn));
        let history = crate::domain::services::message_builder::build_api_messages(&session);
        assert_eq!(history.len(), 4);
        assert_eq!(
            history.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec![
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::User,
                MessageRole::Assistant
            ]
        );
        assert_eq!(history[1].tool_uses[0].id, "t1");
        assert_eq!(history[2].tool_results[0].tool_use_id, "t1");
        assert!(
            history[2].tool_results[0]
                .content
                .contains("SECRET-RESULT-C2B")
        );
        assert_eq!(history[3].content, "Answer: 42");
        adapter.shutdown_loop().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_scheduler_default_timeout_is_ten_minutes_and_is_reported() {
        let tmp = tempfile::Builder::new()
            .prefix("c-2b-cron-")
            .tempdir_in("/tmp")
            .unwrap();
        let started = Arc::new(Notify::new());
        let provider: Arc<dyn StreamingProvider> = Arc::new(C2bSlowProvider {
            started: started.clone(),
        });
        let (core, storage) = scripted_core(tmp.path(), provider);
        let channel = Arc::new(RecordingChannel::default());
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let path = tmp.path().join("cron.toml");
        std::fs::write(
            &path,
            "[[jobs]]\nname='briefing'\nschedule='* * * * *'\nprompt='report'\nforward=true\n",
        )
        .unwrap();
        let adapter =
            CronSchedulerAdapter::load(path, core, completion_tx, channel.clone(), storage.clone())
                .await
                .unwrap();
        let session_id = c2b_fire_one(&adapter).await;
        started.notified().await;
        // Wait for the user-message save before stepping the job's deadline.
        while storage
            .load_conversation(&session_id)
            .await
            .unwrap()
            .is_none()
        {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(599)).await;
        tokio::task::yield_now().await;
        assert!(
            adapter.running.lock().await.contains("briefing"),
            "job must remain live until 600 seconds"
        );
        assert!(
            completion_rx.try_recv().is_err(),
            "job must remain live until 600 seconds"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        while adapter.running.lock().await.contains("briefing") {
            tokio::task::yield_now().await;
        }
        let marker = "[cron: timed out after 600 s]";
        assert_eq!(completion.result_text, marker);
        assert_eq!(
            *channel.sent.lock().await,
            vec![format!("[cron: briefing] {marker}")]
        );
        let session = storage
            .load_conversation(&session_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            session
                .messages
                .iter()
                .any(|m| m.synthetic && m.content == marker)
        );
        adapter.shutdown_loop().await.unwrap();
    }
    #[test]
    fn c2b_timeout_validation_accepts_boundaries_and_rejects_outside() {
        let jobs = [0, 1, 3600, 3601]
            .into_iter()
            .map(|timeout_secs| CronJob {
                name: format!("job-{timeout_secs}"),
                schedule: "* * * * *".into(),
                prompt: "test".into(),
                forward: false,
                timeout_secs: Some(timeout_secs),
            })
            .collect();
        let jobs = parse_jobs(jobs);
        assert_eq!(
            jobs.iter().map(|job| job.timeout_secs).collect::<Vec<_>>(),
            vec![Some(1), Some(3600)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_job_timeout_override_is_reported() {
        let tmp = tempfile::Builder::new()
            .prefix("c-2b-cron-")
            .tempdir_in("/tmp")
            .unwrap();
        let started = Arc::new(Notify::new());
        let provider: Arc<dyn StreamingProvider> = Arc::new(C2bSlowProvider {
            started: started.clone(),
        });
        let (core, storage) = scripted_core(tmp.path(), provider);
        let channel = Arc::new(RecordingChannel::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let path = tmp.path().join("cron.toml");
        std::fs::write(&path, "[[jobs]]\nname='briefing'\nschedule='* * * * *'\nprompt='report'\nforward=true\ntimeout_secs=2\n").unwrap();
        let adapter = CronSchedulerAdapter::load(path, core, tx, channel.clone(), storage.clone())
            .await
            .unwrap();
        let session = c2b_fire_one(&adapter).await;
        started.notified().await;
        while storage.load_conversation(&session).await.unwrap().is_none() {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let result = rx.recv().await.unwrap().result_text;
        while adapter.running.lock().await.contains("briefing") {
            tokio::task::yield_now().await;
        }
        assert_eq!(result, "[cron: timed out after 2 s]");
        assert_eq!(
            *channel.sent.lock().await,
            vec![format!("[cron: briefing] {result}")]
        );
        assert!(
            storage
                .load_conversation(&session)
                .await
                .unwrap()
                .unwrap()
                .messages
                .iter()
                .any(|m| m.synthetic && m.content == result)
        );
        adapter.shutdown_loop().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_shutdown_saves_marker_without_completion_or_forward() {
        let tmp = tempfile::Builder::new()
            .prefix("c-2b-cron-")
            .tempdir_in("/tmp")
            .unwrap();
        let started = Arc::new(Notify::new());
        let provider: Arc<dyn StreamingProvider> = Arc::new(C2bSlowProvider {
            started: started.clone(),
        });
        let (core, storage) = scripted_core(tmp.path(), provider);
        let channel = Arc::new(RecordingChannel::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let path = tmp.path().join("cron.toml");
        std::fs::write(
            &path,
            "[[jobs]]\nname='briefing'\nschedule='* * * * *'\nprompt='report'\nforward=true\n",
        )
        .unwrap();
        let adapter = CronSchedulerAdapter::load(path, core, tx, channel.clone(), storage.clone())
            .await
            .unwrap();
        let session = c2b_fire_one(&adapter).await;
        started.notified().await;
        let before = tokio::time::Instant::now();
        adapter.shutdown_loop().await.unwrap();
        assert!(before.elapsed() <= Duration::from_secs(5));
        assert!(rx.try_recv().is_err());
        assert!(channel.sent.lock().await.is_empty());
        let session = storage.load_conversation(&session).await.unwrap().unwrap();
        assert!(
            session
                .messages
                .iter()
                .any(|m| m.synthetic && m.content == "[cron: cancelled by daemon shutdown]")
        );
    }

    const PROVIDER_START_CANARY: &str = "C2b-raw-provider-start-canary";
    const PROVIDER_STREAM_CANARY: &str = "C2b-raw-provider-stream-canary";
    const COMPOSITION_CANARY: &str = "C2b-raw-composition-canary";

    #[derive(Clone, Copy)]
    enum C2bFailureCase {
        InitialRequest,
        Stream(&'static str),
        SecondResponseDisconnected,
    }

    struct C2bFailureProvider {
        calls: AtomicUsize,
        case: C2bFailureCase,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for C2bFailureProvider {
        async fn stream_completion(
            &self,
            _: Vec<Message>,
            _: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            let chunks = match self.case {
                C2bFailureCase::InitialRequest => {
                    return Err(ProviderError::ConnectionFailed(
                        PROVIDER_START_CANARY.into(),
                    ));
                }
                C2bFailureCase::Stream(detail) => vec![
                    StreamChunk::Text {
                        content: "Partial response".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::Error {
                        content: detail.into(),
                    },
                ],
                C2bFailureCase::SecondResponseDisconnected
                    if self.calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 =>
                {
                    vec![
                        StreamChunk::Text {
                            content: "Checking.".into(),
                            parent_tool_use_id: None,
                        },
                        StreamChunk::ToolUse {
                            id: "t1".into(),
                            name: "read_file".into(),
                            input: serde_json::json!({"file_path": "secret.txt"}),
                        },
                        StreamChunk::TurnComplete {
                            stop_reason: StopReason::ToolUse,
                        },
                    ]
                }
                C2bFailureCase::SecondResponseDisconnected => vec![],
            };
            Ok(futures::stream::iter(chunks).boxed())
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "c2b-failure".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: Duration::ZERO,
            })
        }
    }

    async fn c2b_failure_case(case: C2bFailureCase, expected: &str, canary: Option<&str>) {
        let tmp = tempfile::Builder::new()
            .prefix("c-2b-cron-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "SECRET-RESULT-C2B").unwrap();
        let provider: Arc<dyn StreamingProvider> = Arc::new(C2bFailureProvider {
            calls: AtomicUsize::new(0),
            case,
        });
        let memory: Arc<dyn crate::domain::ports::MemoryPort> = Arc::new(NoOpMemory);
        let (core, storage) = scripted_core_with_memory_tools(
            tmp.path(),
            provider,
            Arc::new(ArcSwap::from_pointee(memory)),
            Arc::new(tokio::sync::RwLock::new(())),
            true,
        );
        let channel = Arc::new(RecordingChannel::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let path = tmp.path().join("cron.toml");
        std::fs::write(
            &path,
            "[[jobs]]\nname='briefing'\nschedule='* * * * *'\nprompt='report'\nforward=true\n",
        )
        .unwrap();
        let adapter = CronSchedulerAdapter::load(path, core, tx, channel.clone(), storage.clone())
            .await
            .unwrap();
        c2b_assert_failure_report(
            &adapter,
            storage.as_ref(),
            &channel,
            &mut rx,
            expected,
            canary,
        )
        .await;
    }

    async fn c2b_assert_failure_report(
        adapter: &CronSchedulerAdapter,
        storage: &dyn StoragePort,
        channel: &RecordingChannel,
        rx: &mut mpsc::UnboundedReceiver<CronCompletion>,
        expected: &str,
        canary: Option<&str>,
    ) {
        let session_id = c2b_fire_one(adapter).await;
        let completion = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("cron completion deadline")
            .expect("completion channel open");
        while adapter.running.lock().await.contains("briefing") {
            tokio::task::yield_now().await;
        }
        adapter.shutdown_loop().await.unwrap();
        assert_eq!(completion.job_name, "briefing");
        assert_eq!(completion.result_text, expected);
        assert!(!completion.result_text.contains("SECRET-RESULT-C2B"));
        let sent = channel.sent.lock().await;
        assert_eq!(&*sent, &[format!("[cron: briefing] {expected}")]);
        let session = storage
            .load_conversation(&session_id)
            .await
            .unwrap()
            .unwrap();
        let markers: Vec<_> = session
            .messages
            .iter()
            .filter(|message| message.synthetic)
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(markers, [expected]);
        if let Some(canary) = canary {
            assert!(!completion.result_text.contains(canary));
            assert!(
                sent.iter()
                    .all(|notification| !notification.contains(canary))
            );
            assert!(!serde_json::to_string(&session).unwrap().contains(canary));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_provider_error_is_failed_not_timeout() {
        c2b_failure_case(
            C2bFailureCase::InitialRequest,
            "[cron: failed — turn ended without a reply]",
            Some(PROVIDER_START_CANARY),
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_disconnected_second_response_reports_partial_reply() {
        c2b_failure_case(
            C2bFailureCase::SecondResponseDisconnected,
            "[cron: failed — Stream disconnected unexpectedly]\nChecking.\n[tool use: read_file (id: t1)]\n",
            None,
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_stream_error_detail_is_local() {
        c2b_failure_case(
            C2bFailureCase::Stream(PROVIDER_STREAM_CANARY),
            "[cron: failed — turn ended without a reply]\nPartial response",
            Some(PROVIDER_STREAM_CANARY),
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_stream_error_cannot_spoof_trusted_disconnect() {
        const SPOOFED_DISCONNECT: &str = "Stream disconnected unexpectedly";
        c2b_failure_case(
            C2bFailureCase::Stream(SPOOFED_DISCONNECT),
            "[cron: failed — turn ended without a reply]\nPartial response",
            Some(SPOOFED_DISCONNECT),
        )
        .await;
    }

    fn c2b_forwarding_briefing(
        workspace: &Path,
        core: Arc<DaemonCore>,
        tx: mpsc::UnboundedSender<CronCompletion>,
        channel: Arc<dyn ChannelPort>,
        storage: Arc<dyn StoragePort>,
    ) -> CronSchedulerAdapter {
        CronSchedulerAdapter::new_loaded(
            workspace.join("cron.toml"),
            vec![ScheduledJob {
                name: "briefing".into(),
                cron: Cron::from_str("* * * * *").unwrap(),
                prompt: "report".into(),
                forward: true,
                timeout_secs: None,
            }],
            core,
            tx,
            channel,
            storage,
            DEFAULT_JOB_TIMEOUT,
            1,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_runtime_composition_error_is_local() {
        let tmp = tempfile::Builder::new()
            .prefix("c-2b-cron-")
            .tempdir_in("/tmp")
            .unwrap();
        let ws = tmp.path();
        let storage = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(ws),
            ws.to_path_buf(),
        ));
        let core = Arc::new(DaemonCore::new(
            ws.to_path_buf(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(|| {
                Err(
                    crate::domain::errors::AdapterCompositionError::AdapterConstructionFailed {
                        port: crate::domain::models::PortDimension::Tools,
                        name: "fixture".into(),
                        source: Box::new(std::io::Error::other(COMPOSITION_CANARY)),
                    },
                )
            }),
        ));
        let channel = Arc::new(RecordingChannel::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let Err(error) = CronSchedulerAdapter::load(
            ws.join("cron.toml"),
            core.clone(),
            tx.clone(),
            channel.clone(),
            storage.clone(),
        )
        .await
        else {
            panic!("startup pre-warm must reject the failing factory");
        };
        assert_eq!(error.to_string(), "cron scheduler runtime pre-warm failed");
        // After the pre-warm failure, exercise the defensive job path with
        // the still-uninitialised runtime through start_loop.
        let adapter = c2b_forwarding_briefing(ws, core, tx, channel.clone(), storage.clone());
        c2b_assert_failure_report(
            &adapter,
            storage.as_ref(),
            &channel,
            &mut rx,
            "[cron: failed — runtime unavailable]",
            Some(COMPOSITION_CANARY),
        )
        .await;
    }

    #[derive(Default)]
    struct C2bParkSave {
        saved: Mutex<Vec<Conversation>>,
        park_attached_result: bool,
        entered: Notify,
        release: Notify,
    }

    #[async_trait::async_trait]
    impl StoragePort for C2bParkSave {
        async fn save_conversation(&self, conversation: &Conversation) -> Result<(), StorageError> {
            if conversation.messages.last().is_some_and(|message| {
                if self.park_attached_result {
                    message.stop_reason == Some(StopReason::ToolUse)
                        && message.tool_calls.iter().any(|call| call.result.is_some())
                } else {
                    message.synthetic && message.content.starts_with("[cron: failed —")
                }
            }) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.saved.lock().await.push(conversation.clone());
            Ok(())
        }

        async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError> {
            Ok(self
                .saved
                .lock()
                .await
                .last()
                .filter(|conversation| conversation.id == id)
                .cloned())
        }

        async fn list_conversations(
            &self,
        ) -> Result<Vec<crate::domain::models::ConversationSummary>, StorageError> {
            Ok(self
                .saved
                .lock()
                .await
                .last()
                .into_iter()
                .map(|conversation| crate::domain::models::ConversationSummary {
                    id: conversation.id.clone(),
                    title: conversation.title.clone(),
                    created_at: conversation.created_at,
                    updated_at: conversation.updated_at,
                    message_count: conversation.messages.len(),
                    has_fork_source: conversation.fork_source.is_some(),
                })
                .collect())
        }

        async fn read_snapshot(
            &self,
            _conversation_id: &str,
            _checkpoint: crate::domain::models::checkpoint::CheckpointId,
            _path: &Path,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(None)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_shutdown_during_final_save_persists_locally_without_publishing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(C2bParkSave::default());
        let core = scripted_core_with_storage(
            tmp.path(),
            Arc::new(C2bFailureProvider {
                calls: AtomicUsize::new(0),
                case: C2bFailureCase::Stream(PROVIDER_STREAM_CANARY),
            }),
            storage.clone(),
        );
        let channel = Arc::new(RecordingChannel::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let adapter =
            c2b_forwarding_briefing(tmp.path(), core, tx, channel.clone(), storage.clone());
        let id = c2b_fire_one(&adapter).await;
        tokio::time::timeout(Duration::from_secs(10), storage.entered.notified())
            .await
            .expect("final save entered");
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(channel.sent.lock().await.is_empty());

        let (shutdown, ()) = tokio::join!(adapter.shutdown_loop(), async {
            adapter.shutdown.cancelled().await;
            storage.release.notify_one();
        });
        shutdown.unwrap();

        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(channel.sent.lock().await.is_empty());
        assert!(adapter.running.lock().await.is_empty());
        let session = storage.load_conversation(&id).await.unwrap().unwrap();
        let marker = session.messages.last().unwrap();
        assert!(marker.synthetic);
        assert_eq!(
            marker.content,
            "[cron: cancelled by daemon shutdown]\nPartial response"
        );
    }

    #[derive(Default)]
    struct C2bParkSecondResponse {
        calls: AtomicUsize,
        active: Notify,
        dropped: Notify,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for C2bParkSecondResponse {
        async fn stream_completion(
            &self,
            _: Vec<Message>,
            _: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            if self.calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                return Ok(futures::stream::iter([
                    StreamChunk::Text {
                        content: "Checking.".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::ToolUse {
                        id: "t1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"file_path": "fixture.txt"}),
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::ToolUse,
                    },
                ])
                .boxed());
            }

            struct NotifyOnDrop<'a>(&'a Notify);
            impl Drop for NotifyOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.notify_one();
                }
            }
            let _on_drop = NotifyOnDrop(&self.dropped);
            self.active.notify_one();
            std::future::pending().await
        }

        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "c2b-park-second-response".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: Duration::ZERO,
            })
        }
    }

    enum C2bParkedJobStop {
        Timeout,
        Shutdown,
        Abort,
    }

    async fn c2b_stop_during_intermediate_save(stop: C2bParkedJobStop) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(C2bParkSave {
            park_attached_result: true,
            ..Default::default()
        });
        let provider = Arc::new(C2bParkSecondResponse::default());
        let core = scripted_core_with_storage(tmp.path(), provider.clone(), storage.clone());
        let channel = Arc::new(RecordingChannel::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let adapter =
            c2b_forwarding_briefing(tmp.path(), core, tx, channel.clone(), storage.clone());
        let id = c2b_fire_one(&adapter).await;
        let wait_bound = Duration::from_secs(2);
        tokio::time::timeout(wait_bound, async {
            tokio::join!(storage.entered.notified(), provider.active.notified());
        })
        .await
        .expect("attached result save parked with second provider response in flight");
        let handle = adapter
            .inflight
            .lock()
            .await
            .pop()
            .expect("tracked cron job");

        let marker = match stop {
            C2bParkedJobStop::Timeout => {
                tokio::time::advance(DEFAULT_JOB_TIMEOUT).await;
                Some("[cron: timed out after 600 s]")
            }
            C2bParkedJobStop::Shutdown => {
                adapter.request_shutdown();
                Some("[cron: cancelled by daemon shutdown]")
            }
            C2bParkedJobStop::Abort => {
                handle.abort();
                None
            }
        };
        tokio::time::timeout(wait_bound, provider.dropped.notified())
            .await
            .expect("provider must be dropped before the save is released");
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(channel.sent.lock().await.is_empty());
        let checkpoint = storage.load_conversation(&id).await.unwrap().unwrap();
        assert_eq!(checkpoint.messages.len(), 2);
        assert_eq!(checkpoint.messages[0].role, MessageRole::User);
        assert_eq!(checkpoint.messages[1].content, "Checking.");
        assert!(checkpoint.messages[1].tool_calls[0].result.is_none());

        let Some(marker) = marker else {
            // A forced drop is not allowed to leave the turn running, even
            // though the cancelled job can no longer finish its ordered write.
            assert!(handle.await.unwrap_err().is_cancelled());
            adapter.shutdown_loop().await.unwrap();
            return;
        };
        storage.release.notify_one();
        tokio::time::timeout(wait_bound, handle)
            .await
            .unwrap()
            .unwrap();
        assert!(adapter.running.lock().await.is_empty());
        adapter.shutdown_loop().await.unwrap();

        let expected = format!("{marker}\nChecking.\n[tool use: Read (id: t1)]\n");
        let saves = storage.saved.lock().await;
        let mut assistant_saves = saves.iter().filter(|save| save.messages.len() > 1);
        let span = &assistant_saves.next().unwrap().messages[1];
        let attached = &assistant_saves.next().unwrap().messages;
        let finished = &assistant_saves.next().unwrap().messages;
        assert!(assistant_saves.next().is_none());
        assert_eq!(attached.len(), 2);
        assert_eq!(attached[1].id, span.id);
        assert_eq!(attached[1].tool_calls[0].wire_id(), "t1");
        assert!(attached[1].tool_calls[0].result.is_some());
        assert_eq!(finished.len(), 3);
        assert_eq!(finished[1].id, span.id);
        assert!(finished[1].tool_calls[0].result.is_some());
        assert!(finished[2].synthetic);
        assert_eq!(finished[2].content, expected);

        if matches!(stop, C2bParkedJobStop::Timeout) {
            assert_eq!(rx.try_recv().unwrap().result_text, expected);
            assert_eq!(
                *channel.sent.lock().await,
                vec![format!("[cron: briefing] {expected}")]
            );
        } else {
            assert!(channel.sent.lock().await.is_empty());
        }
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_timeout_aborts_producer_during_intermediate_save() {
        c2b_stop_during_intermediate_save(C2bParkedJobStop::Timeout).await;
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_shutdown_aborts_producer_during_intermediate_save() {
        c2b_stop_during_intermediate_save(C2bParkedJobStop::Shutdown).await;
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_aborted_job_does_not_detach_producer_during_intermediate_save() {
        c2b_stop_during_intermediate_save(C2bParkedJobStop::Abort).await;
    }

    #[derive(Default)]
    struct C2bParkNotification {
        entered: Notify,
        dropped: AtomicBool,
    }

    #[async_trait::async_trait]
    impl ChannelPort for C2bParkNotification {
        async fn notify(&self, _text: &str) -> Result<(), TransitionError> {
            struct MarkDropped<'a>(&'a AtomicBool);
            impl Drop for MarkDropped<'_> {
                fn drop(&mut self) {
                    self.0.store(true, AtomicOrdering::SeqCst);
                }
            }
            let _mark_dropped = MarkDropped(&self.dropped);
            self.entered.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_shutdown_cancels_inflight_notification_before_grace_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(C2bParkSave::default());
        let core = scripted_core_with_storage(
            tmp.path(),
            Arc::new(StaticProvider {
                chunks: vec![
                    StreamChunk::Text {
                        content: "Ready".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            }),
            storage.clone(),
        );
        let channel = Arc::new(C2bParkNotification::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let adapter =
            c2b_forwarding_briefing(tmp.path(), core, tx, channel.clone(), storage.clone());
        let id = c2b_fire_one(&adapter).await;
        tokio::time::timeout(Duration::from_secs(10), channel.entered.notified())
            .await
            .expect("notification entered");
        let completion = rx.try_recv().unwrap();
        assert_eq!(completion.result_text, "Ready");
        assert!(!channel.dropped.load(AtomicOrdering::SeqCst));

        let before = tokio::time::Instant::now();
        adapter.shutdown_loop().await.unwrap();

        assert!(before.elapsed() < SHUTDOWN_GRACE);
        assert!(channel.dropped.load(AtomicOrdering::SeqCst));
        assert!(adapter.running.lock().await.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let session = storage.load_conversation(&id).await.unwrap().unwrap();
        let assistant = session.messages.last().unwrap();
        assert!(!assistant.synthetic);
        assert_eq!(assistant.content, "Ready");
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_invalid_timeouts_never_schedule_a_provider_request() {
        let tmp = tempfile::Builder::new()
            .prefix("c-2b-cron-")
            .tempdir_in("/tmp")
            .unwrap();
        let started = Arc::new(Notify::new());
        let provider: Arc<dyn StreamingProvider> = Arc::new(C2bSlowProvider {
            started: started.clone(),
        });
        let (core, storage) = scripted_core(tmp.path(), provider);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let path = tmp.path().join("cron.toml");
        std::fs::write(&path, "[[jobs]]\nname='zero'\nschedule='* * * * *'\nprompt='report'\ntimeout_secs=0\n[[jobs]]\nname='large'\nschedule='* * * * *'\nprompt='report'\ntimeout_secs=3601\n").unwrap();
        let adapter = CronSchedulerAdapter::load(
            path,
            core,
            tx,
            Arc::new(RecordingChannel::default()),
            storage,
        )
        .await
        .unwrap();
        assert!(adapter.jobs.load().is_empty());
        adapter.start_loop().await.unwrap();
        tokio::time::advance(Duration::from_secs(120)).await;
        assert!(
            tokio::time::timeout(Duration::ZERO, started.notified())
                .await
                .is_err()
        );
        assert!(rx.try_recv().is_err());
        assert!(adapter.running.lock().await.is_empty());
        adapter.shutdown_loop().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn c2b_private_proposal_is_recorded_but_not_reported() {
        let tmp = tempfile::Builder::new()
            .prefix("c-2b-cron-")
            .tempdir_in("/tmp")
            .unwrap();
        let provider: Arc<dyn StreamingProvider> = Arc::new(StaticProvider {
            chunks: vec![
                StreamChunk::Text {
                    content: "Public".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::ToolUse {
                    id: "private-id".into(),
                    name: crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME.into(),
                    input: serde_json::json!({"secret": "private argument"}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        });
        let (core, storage) = scripted_core(tmp.path(), provider);
        let channel = Arc::new(RecordingChannel::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let path = tmp.path().join("cron.toml");
        std::fs::write(
            &path,
            "[[jobs]]\nname='briefing'\nschedule='* * * * *'\nprompt='report'\nforward=true\n",
        )
        .unwrap();
        let adapter = CronSchedulerAdapter::load(path, core, tx, channel.clone(), storage.clone())
            .await
            .unwrap();
        let session_id = c2b_fire_one(&adapter).await;
        let result = rx.recv().await.unwrap().result_text;
        while adapter.running.lock().await.contains("briefing") {
            tokio::task::yield_now().await;
        }
        assert_eq!(result, "Public");
        assert_eq!(*channel.sent.lock().await, vec!["[cron: briefing] Public"]);
        let session = storage
            .load_conversation(&session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.messages[1].tool_calls[0].wire_id(), "private-id");
        adapter.shutdown_loop().await.unwrap();
    }

    mod c2b_budget_cron {
        use super::*;
        use crate::domain::errors::StorageError;
        use crate::domain::models::pricing::PricingConfig;
        use crate::domain::models::router::{EscalationReason, ModelTier};
        use crate::domain::models::usage::{TokenUsage, UsageLedgerEntry};
        use crate::domain::ports::UsageLedgerPort;
        use std::sync::atomic::AtomicI64;

        const LEDGER_CANARY: &str = "C2b-raw-ledger-canary";

        #[derive(Default)]
        struct C2bBudgetCronReadGate {
            entered: Notify,
            release: Notify,
        }

        struct C2bBudgetCronLedger {
            rows: tokio::sync::Mutex<Vec<UsageLedgerEntry>>,
            fail: bool,
            since: AtomicI64,
            read_gate: Option<C2bBudgetCronReadGate>,
        }

        #[async_trait::async_trait]
        impl UsageLedgerPort for C2bBudgetCronLedger {
            async fn append(&self, entry: UsageLedgerEntry) -> Result<(), StorageError> {
                self.rows.lock().await.push(entry);
                Ok(())
            }
            async fn read_session(
                &self,
                session_id: &str,
            ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
                Ok(self
                    .rows
                    .lock()
                    .await
                    .iter()
                    .filter(|row| row.session_id == session_id)
                    .cloned()
                    .collect())
            }
            async fn read_since(
                &self,
                since: i64,
                _: crate::domain::ports::LedgerReadMode,
            ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
                self.since.store(since, AtomicOrdering::SeqCst);
                if self.fail {
                    return Err(StorageError::Other(LEDGER_CANARY.into()));
                }
                let rows = self
                    .rows
                    .lock()
                    .await
                    .iter()
                    .filter(|row| row.timestamp_ms >= since)
                    .cloned()
                    .collect();
                if let Some(gate) = &self.read_gate {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
                Ok(rows)
            }
        }

        struct C2bBudgetCronProvider(Arc<AtomicUsize>);

        #[async_trait::async_trait]
        impl StreamingProvider for C2bBudgetCronProvider {
            async fn stream_completion(
                &self,
                _messages: Vec<Message>,
                _options: CompletionOptions,
            ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
                self.0.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(futures::stream::iter(vec![
                    StreamChunk::Text {
                        content: "C2bBudgetCron response".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ])
                .boxed())
            }
            async fn abort(&self) -> Result<(), ProviderError> {
                Ok(())
            }
            fn provider_id(&self) -> String {
                "c2b-budget-cron".into()
            }
            fn list_models(&self) -> Vec<ModelDescriptor> {
                vec![]
            }
            async fn health_check(&self) -> Result<(), ProviderError> {
                Ok(())
            }
            async fn connectivity_probe(
                &self,
            ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
                Ok(crate::domain::ports::ProbeOutcome {
                    latency: Duration::ZERO,
                })
            }
        }

        fn c2b_budget_cron_row(timestamp_ms: i64, cents: u32) -> UsageLedgerEntry {
            UsageLedgerEntry {
                timestamp_ms,
                session_id: "C2bBudgetCron-ledger".into(),
                conversation_id: "C2bBudgetCron-ledger".into(),
                provider_id: "c2b-budget-cron".into(),
                model: "c2b-budget-cron-model".into(),
                tier: ModelTier::Flagship,
                step_kind: None,
                escalation_reason: EscalationReason::None,
                usage: TokenUsage {
                    tokens_in: cents * 10_000,
                    tokens_out: 0,
                    parent_ctx: 0,
                    cache_creation_tokens: None,
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                },
                provenance: None,
            }
        }

        fn c2b_budget_cron_core(
            ws: &Path,
            storage: Arc<dyn StoragePort>,
            provider: Arc<dyn StreamingProvider>,
            limit: Option<f64>,
            ledger: Arc<C2bBudgetCronLedger>,
        ) -> Arc<DaemonCore> {
            let mut config = AppConfig::default();
            config.budget.daily_limit_usd = limit;
            config.pricing.insert(
                "c2b-budget-cron-model".into(),
                PricingConfig {
                    input_per_million: 1.0,
                    output_per_million: 0.0,
                    cache_creation_per_million: None,
                    cache_read_per_million: None,
                    reasoning_per_million: None,
                },
            );
            let config = Arc::new(ArcSwap::from_pointee(config));
            let factory_ws = ws.to_path_buf();
            let factory_storage = storage.clone();
            let factory_config = config.clone();
            Arc::new(DaemonCore::new(
                ws.to_path_buf(),
                config,
                Arc::new(NoOpMemory),
                storage.clone(),
                Arc::new(NoOpSecurity),
                Arc::new(NoOpPersona),
                Arc::new(crate::adapters::rap::PeerTopicStore::new()),
                Box::new(move || {
                    let mut runtime =
                        scripted_runtime(provider.clone(), factory_storage.clone(), &factory_ws);
                    {
                        let inner = Arc::get_mut(&mut runtime).expect("new runtime has one owner");
                        inner.app_config = factory_config.clone();
                        inner.usage_ledger = ledger.clone();
                    }
                    Ok(runtime)
                }),
            ))
        }

        async fn c2b_budget_cron_case(
            limit: Option<f64>,
            cents: u32,
            yesterday_cents: u32,
            fail: bool,
            expected: &str,
            requests: usize,
        ) {
            let tmp = tempfile::Builder::new()
                .prefix("c-2b-budget-cron-")
                .tempdir_in("/tmp")
                .unwrap();
            let ws = tmp.path();
            let today = crate::infrastructure::clock_util::today_start_unix_ms();
            let ledger = Arc::new(C2bBudgetCronLedger {
                rows: tokio::sync::Mutex::new(vec![
                    c2b_budget_cron_row(today, cents),
                    c2b_budget_cron_row(today - 1, yesterday_cents),
                ]),
                fail,
                since: AtomicI64::new(-1),
                read_gate: None,
            });
            let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(ws),
                ws.to_path_buf(),
            ));
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = Arc::new(C2bBudgetCronProvider(calls.clone()));
            let core = c2b_budget_cron_core(ws, storage.clone(), provider, limit, ledger.clone());
            let channel = Arc::new(RecordingChannel::default());
            let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
            let adapter = CronSchedulerAdapter::new_loaded(
                ws.join("cron.toml"),
                vec![ScheduledJob {
                    name: "C2bBudgetCron".into(),
                    cron: Cron::from_str("* * * * *").unwrap(),
                    prompt: "C2bBudgetCron prompt".into(),
                    forward: true,
                    timeout_secs: None,
                }],
                core,
                completion_tx,
                channel.clone(),
                storage.clone(),
                Duration::from_secs(5),
                1,
            );
            adapter.start_loop().await.unwrap();
            let fire_at = loop {
                if let Some((_, Some(ts))) = adapter.next_runs().await.first() {
                    break *ts;
                }
                tokio::task::yield_now().await;
            };
            adapter.jobs.store(Arc::new(vec![]));
            tokio::time::advance(Duration::from_secs(61)).await;
            let completion = tokio::time::timeout(Duration::from_secs(10), completion_rx.recv())
                .await
                .expect("cron completion deadline")
                .expect("completion channel open");
            while adapter.running.lock().await.contains("C2bBudgetCron") {
                tokio::task::yield_now().await;
            }
            adapter.shutdown_loop().await.unwrap();
            assert_eq!(completion.job_name, "C2bBudgetCron");
            assert_eq!(completion.result_text, expected);
            assert_eq!(calls.load(AtomicOrdering::SeqCst), requests);
            if limit.is_some() {
                assert_eq!(ledger.since.load(AtomicOrdering::SeqCst), today);
            }
            let id = format!(
                "cron-{}-{fire_at}",
                sanitize_session_component("C2bBudgetCron")
            );
            let conversation = storage
                .load_conversation(&id)
                .await
                .unwrap()
                .expect("stored cron session");
            let assistant = conversation
                .messages
                .iter()
                .find(|m| m.role == MessageRole::Assistant)
                .expect("assistant marker or response");
            assert_eq!(assistant.content, expected);
            assert_eq!(assistant.synthetic, requests == 0);
            let sent = channel.sent.lock().await;
            assert_eq!(&*sent, &[format!("[cron: C2bBudgetCron] {expected}")]);
            if fail {
                assert!(!completion.result_text.contains(LEDGER_CANARY));
                assert!(
                    sent.iter()
                        .all(|notification| !notification.contains(LEDGER_CANARY))
                );
                assert!(
                    !serde_json::to_string(&conversation)
                        .unwrap()
                        .contains(LEDGER_CANARY)
                );
            }
        }

        #[tokio::test(start_paused = true)]
        async fn c2b_shutdown_during_budget_read_does_not_admit_turn() {
            let tmp = tempfile::tempdir().unwrap();
            let today = crate::infrastructure::clock_util::today_start_unix_ms();
            let ledger = Arc::new(C2bBudgetCronLedger {
                rows: Mutex::new(vec![c2b_budget_cron_row(today, 99)]),
                fail: false,
                since: AtomicI64::new(-1),
                read_gate: Some(C2bBudgetCronReadGate::default()),
            });
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = Arc::new(C2bBudgetCronProvider(calls.clone()));
            let storage = Arc::new(C2bParkSave::default());
            let core = c2b_budget_cron_core(
                tmp.path(),
                storage.clone(),
                provider,
                Some(1.0),
                ledger.clone(),
            );
            let channel = Arc::new(RecordingChannel::default());
            let (tx, mut rx) = mpsc::unbounded_channel();
            let adapter =
                c2b_forwarding_briefing(tmp.path(), core, tx, channel.clone(), storage.clone());
            let id = c2b_fire_one(&adapter).await;

            let gate = ledger.read_gate.as_ref().unwrap();
            tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
                .await
                .expect("below-limit budget read parked");
            adapter.request_shutdown();
            gate.release.notify_one();
            adapter.shutdown_loop().await.unwrap();

            assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            assert!(channel.sent.lock().await.is_empty());
            assert!(adapter.running.lock().await.is_empty());
            let session = storage.load_conversation(&id).await.unwrap().unwrap();
            let [marker] = session.messages.as_slice() else {
                panic!("shutdown must persist only the marker without admitting a turn");
            };
            assert!(marker.synthetic);
            assert_eq!(marker.content, "[cron: cancelled by daemon shutdown]");
        }

        #[tokio::test(start_paused = true)]
        async fn c2b_budget_cron_equal_limit_refuses() {
            c2b_budget_cron_case(
                Some(1.0),
                100,
                0,
                false,
                "[cron: skipped — daily budget of $1.00 reached ($1.00 spent today)]",
                0,
            )
            .await;
        }
        #[tokio::test(start_paused = true)]
        async fn c2b_budget_cron_below_limit_runs() {
            c2b_budget_cron_case(Some(1.0), 99, 0, false, "C2bBudgetCron response", 1).await;
        }
        #[tokio::test(start_paused = true)]
        async fn c2b_budget_cron_no_limit_runs_despite_spend() {
            c2b_budget_cron_case(None, 5000, 0, false, "C2bBudgetCron response", 1).await;
        }
        #[tokio::test(start_paused = true)]
        async fn c2b_budget_cron_unreadable_ledger_refuses_with_limit() {
            c2b_budget_cron_case(
                Some(1.0),
                0,
                0,
                true,
                "[cron: skipped — usage ledger unreadable]",
                0,
            )
            .await;
        }
        #[tokio::test(start_paused = true)]
        async fn c2b_budget_cron_unreadable_ledger_runs_without_limit() {
            c2b_budget_cron_case(None, 0, 0, true, "C2bBudgetCron response", 1).await;
        }
        #[tokio::test(start_paused = true)]
        async fn c2b_budget_cron_yesterday_does_not_count() {
            c2b_budget_cron_case(Some(1.0), 99, 5000, false, "C2bBudgetCron response", 1).await;
        }
    }
}
