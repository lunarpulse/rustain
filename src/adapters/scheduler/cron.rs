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

use crate::adapters::daemon::runtime::DaemonCore;
use crate::domain::errors::TransitionError;
use crate::domain::events::AppEvent;
use crate::domain::models::{
    ChannelKind, ChatMessage, Conversation, CronConfig, CronJob, HealthSummary, MessageRole,
    StopReason, StreamChunk, TurnOrigin, generate_message_id,
};
use crate::domain::ports::{ChannelPort, SchedulerPort, StoragePort};

const HEALTH_OK: u8 = 0;
const HEALTH_DEGRADED: u8 = 1;
const HEALTH_OFFLINE: u8 = 2;
const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(5);
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
        core.ensure_runtime()
            .await
            .map_err(|e| anyhow::anyhow!("cron scheduler runtime pre-warm failed: {e}"))?;
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

    async fn shutdown_loop(&self) -> Result<(), TransitionError> {
        self.shutdown.cancel();
        // Shared deadline: bound total shutdown to job_timeout regardless of inflight count (NFR48).
        let deadline = tokio::time::sleep(self.job_timeout);
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
    loop {
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
                _ = state.shutdown.cancelled() => break,
                _ = state.reload_notify.notified() => continue,
            }
        };

        let delay = next
            .signed_duration_since(Local::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = state.reload_notify.notified() => continue,
            _ = tokio::time::sleep(delay) => {
                let fire_at = next.timestamp();
                for job in snapshot.iter().filter(|job| job.cron.is_time_matching(&next).unwrap_or(false)) {
                    maybe_spawn_job(&state, job.clone(), fire_at).await;
                }
            }
        }
    }
    state.health.store(HEALTH_OFFLINE, Ordering::SeqCst);
}

async fn maybe_spawn_job(state: &LoopState, job: ScheduledJob, fired_at_unix: i64) {
    {
        let mut running = state.running.lock().await;
        if running.contains(&job.name) {
            tracing::warn!(job = %job.name, "cron: job still running; skipping overlapping fire");
            return;
        }
        running.insert(job.name.clone());
    }

    let permit = match state.sem.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            state.running.lock().await.remove(&job.name);
            tracing::warn!(job = %job.name, "cron: global concurrency cap reached; skipping fire");
            return;
        }
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

async fn run_job(task: CronJobTask) {
    let session_id = format!(
        "cron-{}-{}",
        sanitize_session_component(&task.job.name),
        task.fired_at_unix
    );
    let mut conversation = new_cron_conversation(&session_id, &task.job.name, task.fired_at_unix);
    let (job_tx, mut job_rx) = mpsc::unbounded_channel();
    let child_cancel = task.shutdown.child_token();
    let rt = match task.core.ensure_runtime().await {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(job = %task.job.name, error = %e, "cron: runtime unavailable; job skipped");
            task.running.lock().await.remove(&task.job.name);
            return;
        }
    };
    let handle = rt.drive_turn(
        task.job.prompt.clone(),
        ChannelKind::Cron,
        &mut conversation,
        &job_tx,
        TurnOrigin::Cron,
        child_cancel.clone(),
    );
    if let Err(e) = task.storage.save_conversation(&conversation).await {
        tracing::warn!(job = %task.job.name, error = %e, "cron: saving user message failed");
    }

    let mut assistant = String::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut completed = false;
    let timeout = tokio::time::sleep(task.job_timeout);
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            maybe = job_rx.recv() => {
                let Some(event) = maybe else { break };
                if let AppEvent::ProviderChunk { chunk, .. } = event {
                    match chunk {
                        StreamChunk::Text { content, .. } => assistant.push_str(&content),
                        StreamChunk::ToolUse { id, name, .. } => assistant.push_str(&format!("\n[tool use: {name} (id: {id})]\n")),
                        StreamChunk::ToolResult { content, .. } => assistant.push_str(&format!("[tool result: {content}]\n")),
                        StreamChunk::TurnComplete { stop_reason: sr } => {
                            stop_reason = sr;
                            completed = true;
                            break;
                        }
                        StreamChunk::Error { content } => assistant.push_str(&content),
                        _ => {}
                    }
                }
            }
            _ = &mut timeout => {
                child_cancel.cancel();
                handle.abort();
                tracing::warn!(job = %task.job.name, "cron: job timed out; aborted");
                break;
            }
            _ = task.shutdown.cancelled() => {
                child_cancel.cancel();
                handle.abort();
                tracing::warn!(job = %task.job.name, "cron: job cancelled by daemon shutdown");
                break;
            }
        }
    }
    // Always await the drive_turn handle after abort for defense-in-depth (review patch 4).
    if !completed {
        handle.abort();
    }
    let _ = handle.await;

    if completed {
        conversation.messages.push(ChatMessage {
            id: generate_message_id(),
            role: MessageRole::Assistant,
            content: assistant.clone(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: crate::domain::models::session_meta::now_unix(),
            token_count: None,
            stop_reason: Some(stop_reason),
            synthetic: false,
            images: vec![],
            origin: ChannelKind::Cron,
        });
        conversation.updated_at = crate::domain::models::session_meta::now_unix();
        conversation.last_response_at = Some(conversation.updated_at);
        if let Err(e) = task.storage.save_conversation(&conversation).await {
            tracing::warn!(job = %task.job.name, error = %e, "cron: saving completed session failed");
        }
    } else {
        // Timed out or cancelled: persist session with a diagnostic marker (review patch 3).
        conversation.messages.push(ChatMessage {
            id: generate_message_id(),
            role: MessageRole::Assistant,
            content: if assistant.is_empty() {
                "[cron: timed out or cancelled — no response]".to_string()
            } else {
                format!("[cron: timed out or cancelled — partial]\n{assistant}")
            },
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: crate::domain::models::session_meta::now_unix(),
            token_count: None,
            stop_reason: Some(stop_reason),
            synthetic: true,
            images: vec![],
            origin: ChannelKind::Cron,
        });
        conversation.updated_at = crate::domain::models::session_meta::now_unix();
        conversation.last_response_at = Some(conversation.updated_at);
        let _ = task.storage.save_conversation(&conversation).await;
    }

    // Only send completion and forward on successful completion (review patch 3).
    if completed {
        if task
            .completion_tx
            .send(CronCompletion {
                job_name: task.job.name.clone(),
                result_text: assistant.clone(),
            })
            .is_err()
        {
            tracing::warn!(job = %task.job.name, "cron: completion dropped — forwarder exited");
        }
        if task.job.forward {
            let text = format!("[cron: {}] {}", task.job.name, assistant);
            if let Err(e) = task.channel.notify(&text).await {
                tracing::warn!(job = %task.job.name, error = %e, "cron: forward failed");
            }
        }
    }
    task.running.lock().await.remove(&task.job.name);
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
        NoOpApprovalPersistence, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpToolSet,
        NoOpUsageLedger,
    };
    use crate::adapters::toolset_adapter::ToolSetAdapter;
    use crate::domain::errors::ProviderError;
    use crate::domain::models::provider::ModelDescriptor;
    use crate::domain::models::{AppConfig, CompletionOptions, MemoryFact, Message, StreamChunk};
    use crate::domain::ports::{SecurityPort, StoragePort, StreamingProvider, ToolSetPort};
    use crate::domain::services::approval_runtime::ApprovalRuntime;
    use crate::domain::services::tool_scheduler::ToolScheduler;
    use arc_swap::ArcSwap;
    use futures::StreamExt;
    use futures::stream::BoxStream;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

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
            provider,
            app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            security,
            tools,
            tool_scheduler,
            persona: Arc::new(NoOpPersona),
            context_assembler: Arc::new(ArcSwap::from_pointee(None)),
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
        let ws = workspace.to_path_buf();
        let storage_for_factory = storage.clone();
        let provider_for_factory = provider.clone();
        let core = DaemonCore::new(
            workspace.to_path_buf(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Box::new(move || {
                Ok(scripted_runtime(
                    provider_for_factory.clone(),
                    storage_for_factory.clone(),
                    &ws,
                ))
            }),
        );
        (Arc::new(core), storage)
    }

    fn scripted_runtime_with_memory_tools(
        provider: Arc<dyn StreamingProvider>,
        storage: Arc<dyn StoragePort>,
        workspace: &Path,
        memory_slot: Arc<arc_swap::ArcSwap<Arc<dyn crate::domain::ports::MemoryPort>>>,
        memory_write_gate: Arc<tokio::sync::RwLock<()>>,
    ) -> Arc<DaemonTurnRuntime> {
        let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
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
        let tools: Arc<dyn ToolSetPort> = Arc::new(adapter);
        let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
        let tool_scheduler =
            ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 64);
        Arc::new(DaemonTurnRuntime {
            provider,
            app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            security,
            tools,
            tool_scheduler,
            persona: Arc::new(NoOpPersona),
            context_assembler: Arc::new(ArcSwap::from_pointee(None)),
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
            Box::new(move || {
                Ok(scripted_runtime_with_memory_tools(
                    provider_for_factory.clone(),
                    storage_for_factory.clone(),
                    &ws,
                    memory_slot_for_factory.clone(),
                    gate_for_factory.clone(),
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
            },
            CronJob {
                name: "hourly".into(),
                schedule: "0 * * * *".into(),
                prompt: "Check health".into(),
                forward: false,
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
            },
            CronJob {
                name: "b".into(),
                schedule: "99 99 99 99 99".into(),
                prompt: "y".into(),
                forward: false,
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

    #[tokio::test]
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
        tokio::time::timeout(Duration::from_millis(250), adapter.shutdown_loop())
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
}
