//! Daemon lifecycle runtime (Story 12.1a Task 3/4/6) — the headless `tokio::select!`
//! loop, the single `SessionBoundary` emit seam (AC-12-1a-7), and the graceful
//! shutdown path (AC-12-1a-3, NFR48).
//!
//! This loop is **new and minimal**. It is to the daemon what `event_loop::run`
//! is to the TUI, but ~1/100th the size — and crucially it is NOT `event_loop::run`,
//! which is TUI-coupled (`&mut Tui` + `crossterm::EventStream`) and cannot run
//! headless (see Story 12.1a Dev Notes §"Headless seam").

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{SignalKind, signal};

use crate::domain::models::{AppConfig, ChatMessage, ConsolidationDueMarker, SessionBoundary};
use crate::domain::ports::{ChannelPort, MemoryPort, RecallProviderPort, SchedulerPort};

use super::{pidfile, session_queue, socket};

/// Everything the lifecycle loop needs. Constructed by `mod::run_daemon_foreground`
/// after composition + socket bind + PID-file write.
pub struct DaemonRuntime {
    pub config: AppConfig,
    pub memory: Arc<dyn MemoryPort>,
    /// Story 12.1c AC4 — the (optional, default `NoopRecallProvider`) external
    /// recall hook, invoked unconditionally at every boundary.
    pub recall: Arc<dyn RecallProviderPort>,
    /// Story 12.1c AC2/AC3 — workspace root for the durable `.rustain/` boundary
    /// queues (consolidation-due marker + purge audit notice).
    pub workspace: PathBuf,
    pub pid_path: PathBuf,
    pub socket_path: PathBuf,
    /// Story 12.2b — the attach server (accept loop + forwarder + approval gate).
    pub server: Arc<super::server::AttachServer>,
    /// Story 12.3 — composed daemon channel adapter (terminal noop or Telegram).
    pub channel: Arc<dyn ChannelPort>,
    /// Story 12.4 — cron scheduler adapter (or NoOpScheduler).
    pub scheduler: Arc<dyn SchedulerPort>,
    /// Story 12.2b — the daemon's per-activation event bus receiver, handed to the
    /// server's single forwarder task. `Option` so `run_lifecycle` can `take()` it.
    pub domain_rx: Option<tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>>,
    /// Story 12.3 — inbound channel-turn queue handed to the attach server.
    pub channel_turn_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::domain::models::ChannelTurnRequest>>,
    /// Story 12.4 — completed cron results injected into the forwarder.
    #[cfg(feature = "cron")]
    pub cron_completion_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::adapters::scheduler::cron::CronCompletion>,
    >,
    /// Story 12.2b — the per-process conversation (origin-tagged transcript). Fed
    /// to `emit_session_boundary` (AC4) and snapshotted on attach.
    pub conversation: Arc<tokio::sync::Mutex<crate::domain::models::Conversation>>,
}

/// THE single shared emit seam — AC-12-1a-7 "one code path, three triggers" +
/// Story 12.1c's AC2/AC3/AC4 hooks.
///
/// All of { `daily_reset` fires, `idle_timeout` fires, graceful shutdown begins }
/// call exactly this function; there are no divergent boundary paths. The boundary
/// action is, in order:
///   1. **finalize the daily log** (drain + flush) — 12.1a, the 12.0 hardened sink.
///   2. **AC4** — `RecallProviderPort::on_session_end(transcript)`, invoked
///      UNCONDITIONALLY (the `Noop` default makes the no-op explicit). Since Story
///      12.2b the daemon HAS a per-process conversation, so the **real** transcript
///      is fed here (it was honestly empty in 12.1a–c, before the message runtime).
///   3. **AC3** — honor `MEMORY.md` hand-deletions LIVE through the SAME 12.0
///      `refresh()` redaction sink (`honor_md_removals`), then queue a durable
///      "N facts removed" audit notice (never silent, never a confirm-gate).
///   4. **AC2** — queue a durable "consolidation-due" marker + daily-log slice ref
///      (no LLM sub-turn headless; 12.2 generates the suggestion + card).
///
/// Every step is best-effort: a failing hook logs and the boundary proceeds (a
/// boundary must not hang or abort on a side-channel write). NONE of this is a
/// parallel path — it all lives in THIS one function body (12.0 AC9 / 12.1a "one
/// code path, three triggers"). Daily logs are NEVER deleted here.
pub async fn emit_session_boundary(
    boundary: SessionBoundary,
    memory: &Arc<dyn MemoryPort>,
    recall: &Arc<dyn RecallProviderPort>,
    workspace: &Path,
    transcript: &[ChatMessage],
) {
    tracing::info!(boundary = %boundary, "daemon SessionBoundary");

    // 1. Finalize the daily log via the 12.0 hardened sink (drain-await-quiescence
    //    then flush). In the headless daemon there is no live message source, so
    //    this is usually a no-op drain — but routing through the same sink keeps
    //    the seam correct once 12.2+ feed messages.
    if let Err(e) = memory.prepare_detach().await {
        tracing::warn!(boundary = %boundary, error = %e, "prepare_detach at boundary failed");
    }
    if let Err(e) = memory.flush().await {
        tracing::warn!(boundary = %boundary, error = %e, "memory flush at boundary failed");
    }

    // 2. AC4 — on_session_end, unconditional with the REAL per-process transcript
    //    (Story 12.2b: the daemon now drives turns, so the conversation is fed
    //    through). The `Noop` default still must not bake in `if empty { return }`.
    tracing::debug!(
        boundary = %boundary,
        transcript_len = transcript.len(),
        "on_session_end invoked with the per-process conversation transcript"
    );
    if let Err(e) = recall.on_session_end(transcript).await {
        tracing::warn!(boundary = %boundary, error = %e, "on_session_end at boundary failed");
    }

    // 3. AC3 — honor MEMORY.md hand-deletions LIVE (hand-edit = consent), then
    //    queue the audit notice. No-op under a non-vector profile (no refresh sink).
    match memory.honor_md_removals().await {
        Ok(removed) if !removed.is_empty() => {
            let summaries: Vec<String> = removed.iter().map(|e| e.summary.clone()).collect();
            tracing::info!(
                boundary = %boundary,
                count = removed.len(),
                "honored MEMORY.md hand-deletions — purged from search index"
            );
            if let Err(e) =
                session_queue::enqueue_purge_notice(workspace, removed.len(), summaries, now_unix())
            {
                tracing::warn!(boundary = %boundary, error = %e, "queuing MEMORY.md purge notice failed");
            }
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(boundary = %boundary, error = %e, "MEMORY.md file-edit honor failed");
        }
    }

    // 4. AC2 — queue a durable consolidation-due marker (latest-only). The daemon
    //    has no provider to GENERATE a suggestion; it records the trigger + the
    //    daily-log slice (today's date) for 12.2 to consolidate.
    //    Use a SINGLE clock sample for both the unix timestamp and the date string
    //    to avoid off-by-one-second gaps (review patch).
    let now = chrono::Local::now();
    let marker = ConsolidationDueMarker {
        boundary: boundary.as_str().to_string(),
        queued_at_unix: now.timestamp().max(0) as u64,
        daily_log_ref: now.format("%Y-%m-%d").to_string(),
    };
    if let Err(e) = session_queue::enqueue_consolidation_due(workspace, &marker) {
        tracing::warn!(boundary = %boundary, error = %e, "queuing consolidation-due marker failed");
    }
}

/// Unix seconds now (saturating). Local helper so the boundary path doesn't depend
fn now_unix() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

/// Pure helper (no clock, no timers — unit-testable per project law
/// "determinism > realism"): how long from `now` until the next occurrence of the
/// wall-clock `target`. If `target` has already passed today, it is tomorrow.
pub fn duration_until_next(now: chrono::NaiveTime, target: chrono::NaiveTime) -> Duration {
    let day = chrono::Duration::days(1);
    let mut delta = target - now;
    if delta <= chrono::Duration::zero() {
        delta += day;
    }
    delta.to_std().unwrap_or(Duration::ZERO)
}

/// Compute the initial daily-reset sleep from the configured wall-clock time,
/// using the real local clock. Thin wrapper over [`duration_until_next`] so the
/// math stays pure and tested.
fn next_daily_reset(target: chrono::NaiveTime) -> Duration {
    duration_until_next(chrono::Local::now().time(), target)
}

/// Run the headless lifecycle loop until a shutdown signal. Returns after the
/// graceful-shutdown path has removed the PID file + socket.
pub async fn run_lifecycle(mut rt: DaemonRuntime) -> Result<()> {
    let idle = rt
        .config
        .daemon
        .parsed_idle_timeout()
        .map_err(|e| anyhow::anyhow!(e))?;
    let daily_target = rt
        .config
        .daemon
        .parsed_daily_reset()
        .map_err(|e| anyhow::anyhow!(e))?;
    let low_power_emits = rt.config.daemon.low_power_emits_boundary;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let listener = socket::bind(&rt.socket_path)?;

    if let Err(e) = rt.channel.start_loop().await {
        tracing::warn!(error = %e, "daemon: channel adapter start_loop failed — continuing without channel");
    }
    if let Err(e) = rt.scheduler.start_loop().await {
        tracing::warn!(error = %e, "daemon: scheduler adapter start_loop failed — continuing without scheduler");
    }

    // Story 12.2b — the attach server owns the accept loop + forwarder. We hand it
    // the listener + the bus receiver and a shutdown token; the lifecycle loop
    // below keeps its timer/signal duties (boundaries + graceful shutdown).
    let server_shutdown = tokio_util::sync::CancellationToken::new();
    let domain_rx = rt
        .domain_rx
        .take()
        .expect("DaemonRuntime.domain_rx must be set");
    let channel_rx = rt.channel_turn_rx.take();
    #[cfg(feature = "cron")]
    let cron_completion_rx = rt.cron_completion_rx.take();
    #[cfg(not(feature = "cron"))]
    let cron_completion_rx = None;
    let server = rt.server.clone();
    let server_handle = tokio::spawn({
        let sd = server_shutdown.clone();
        async move {
            server
                .run(listener, domain_rx, channel_rx, cron_completion_rx, sd)
                .await
        }
    });

    // Timers. The idle timer arm is disabled (via the `if !low_power` guard) once
    // we are already in low-power, and re-armed on activity.
    let mut daily_timer = Box::pin(tokio::time::sleep(next_daily_reset(daily_target)));
    let mut idle_timer = Box::pin(tokio::time::sleep(idle));
    let mut low_power = false;

    tracing::info!(
        idle_secs = idle.as_secs(),
        daily_reset = %rt.config.daemon.daily_reset,
        "daemon lifecycle ready (Story 12.4: attach server + scheduler live, headless turn runtime lazy)"
    );

    loop {
        tokio::select! {
            _ = sigterm.recv() => { tracing::info!("daemon received SIGTERM"); break; }
            _ = sigint.recv()  => { tracing::info!("daemon received SIGINT");  break; }
            _ = sighup.recv()  => {
                tracing::info!("daemon received SIGHUP; reloading scheduler");
                if let Err(e) = rt.scheduler.reload().await {
                    tracing::warn!(error = %e, "daemon: scheduler reload failed");
                }
            }

            _ = &mut daily_timer => {
                let transcript = { rt.conversation.lock().await.messages.clone() };
                if tokio::time::timeout(
                    Duration::from_secs(2),
                    emit_session_boundary(
                        SessionBoundary::DailyReset,
                        &rt.memory,
                        &rt.recall,
                        &rt.workspace,
                        &transcript,
                    ),
                )
                .await
                .is_err()
                {
                    tracing::warn!("daemon: daily_reset boundary emit timed out (>2s)");
                }
                low_power = false;
                daily_timer = Box::pin(tokio::time::sleep(next_daily_reset(daily_target)));
                idle_timer = Box::pin(tokio::time::sleep(idle));
            }

            _ = &mut idle_timer, if !low_power => {
                low_power = true;
                tracing::info!("daemon entered low-power state");
                if low_power_emits {
                    let transcript = { rt.conversation.lock().await.messages.clone() };
                    if tokio::time::timeout(
                        Duration::from_secs(2),
                        emit_session_boundary(
                            SessionBoundary::IdleTimeout,
                            &rt.memory,
                            &rt.recall,
                            &rt.workspace,
                            &transcript,
                        ),
                    )
                    .await
                    .is_err()
                    {
                        tracing::warn!("daemon: idle_timeout boundary emit timed out (>2s)");
                    }
                }
            }
        }
    }

    server_shutdown.cancel();
    server_handle.abort();
    graceful_shutdown(&rt).await;
    Ok(())
}

/// Graceful shutdown (AC-12-1a-3, NFR48 < 5s). Emits the `Shutdown` boundary
/// through the single seam, then removes the PID file + socket. Each potentially
/// blocking step is wrapped in a per-step `tokio::time::timeout` (mirroring the
/// 2s-per-step pattern at `event_loop.rs:7965-8005`) so the total stays bounded;
/// on overrun we log and proceed to exit rather than hang.
async fn graceful_shutdown(rt: &DaemonRuntime) {
    tracing::info!("daemon graceful shutdown begin");

    // Boundary emit (drain + flush) — capped so a stuck flush can't blow NFR48.
    let transcript = { rt.conversation.lock().await.messages.clone() };
    if tokio::time::timeout(
        Duration::from_secs(3),
        emit_session_boundary(
            SessionBoundary::Shutdown,
            &rt.memory,
            &rt.recall,
            &rt.workspace,
            &transcript,
        ),
    )
    .await
    .is_err()
    {
        tracing::warn!("daemon shutdown: boundary flush timed out (>3s), proceeding to exit");
    }

    // Stop cold-tier loops before removing runtime files.
    if let Err(e) = rt.scheduler.shutdown_loop().await {
        tracing::warn!(error = %e, "daemon: scheduler adapter shutdown_loop failed");
    }
    if let Err(e) = rt.channel.shutdown_loop().await {
        tracing::warn!(error = %e, "daemon: channel adapter shutdown_loop failed");
    }

    socket::cleanup(&rt.socket_path);
    pidfile::remove(&rt.pid_path);
    tracing::info!("daemon graceful shutdown complete");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::domain::errors::{MemoryError, TransitionError};
    use crate::domain::models::{ChatMessage, MemoryEntry, TransitionState};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Recording memory adapter — counts the sink calls so we can prove all three
    /// triggers reach ONE shared path (AC-12-1a-7) and each drives the AC3
    /// `honor_md_removals` hook exactly once (Story 12.1c). Deterministic, no sleep.
    #[derive(Default)]
    struct RecordingMemory {
        prepare_detach_calls: AtomicUsize,
        flush_calls: AtomicUsize,
        honor_calls: AtomicUsize,
        /// Cumulative count of entries returned across all `honor_md_removals` calls.
        /// Guards against a bug where early boundaries silently drop removals
        /// while still passing the per-boundary `honor_calls` assert.
        total_honored: AtomicUsize,
        /// When set, `honor_md_removals` reports this many purged entries so the
        /// boundary's AC3 audit-notice queue path is exercised.
        honor_yields: usize,
    }
    #[async_trait::async_trait]
    impl MemoryPort for RecordingMemory {
        async fn flush(&self) -> Result<(), MemoryError> {
            self.flush_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn prepare_detach(&self) -> Result<TransitionState, TransitionError> {
            self.prepare_detach_calls.fetch_add(1, Ordering::SeqCst);
            Ok(TransitionState::empty("memory"))
        }
        async fn honor_md_removals(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
            self.honor_calls.fetch_add(1, Ordering::SeqCst);
            let entries: Vec<MemoryEntry> = (0..self.honor_yields)
                .map(|i| MemoryEntry {
                    timestamp: chrono::Local::now(),
                    summary: format!("removed fact {i}"),
                    context: None,
                })
                .collect();
            self.total_honored
                .fetch_add(entries.len(), Ordering::SeqCst);
            Ok(entries)
        }
    }

    /// Recording recall provider (Story 12.1c AC4) — counts `on_session_end` and
    /// asserts the transcript is the headless empty slice. AtomicUsize, no sleeps.
    #[derive(Default)]
    struct RecordingRecall {
        calls: AtomicUsize,
        max_seen_len: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl RecallProviderPort for RecordingRecall {
        async fn on_session_end(
            &self,
            transcript: &[ChatMessage],
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.max_seen_len
                .fetch_max(transcript.len(), Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn all_three_triggers_route_through_one_sink() {
        let ws = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingMemory {
            honor_yields: 2, // each boundary "purges" 2 → exercises the notice queue
            ..Default::default()
        });
        let mem: Arc<dyn MemoryPort> = rec.clone();
        let recall_rec = Arc::new(RecordingRecall::default());
        let recall: Arc<dyn RecallProviderPort> = recall_rec.clone();

        emit_session_boundary(SessionBoundary::DailyReset, &mem, &recall, ws.path(), &[]).await;
        emit_session_boundary(SessionBoundary::IdleTimeout, &mem, &recall, ws.path(), &[]).await;
        emit_session_boundary(SessionBoundary::Shutdown, &mem, &recall, ws.path(), &[]).await;

        // Each of the three boundaries drove the SAME sink + EACH 12.1c hook exactly
        // once → one path, three triggers (AC1).
        assert_eq!(rec.prepare_detach_calls.load(Ordering::SeqCst), 3);
        assert_eq!(rec.flush_calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            rec.honor_calls.load(Ordering::SeqCst),
            3,
            "AC3 honor per boundary"
        );
        assert_eq!(
            rec.total_honored.load(Ordering::SeqCst),
            6,
            "every boundary's removals were actually honored (not just the last)"
        );
        assert_eq!(
            recall_rec.calls.load(Ordering::SeqCst),
            3,
            "AC4 on_session_end per boundary"
        );
        // AC4: the headless transcript is always empty (no source until 12.2).
        assert_eq!(recall_rec.max_seen_len.load(Ordering::SeqCst), 0);

        // AC2: a durable consolidation-due marker was queued (latest-only — the
        // last boundary, Shutdown, wins).
        let marker = session_queue::read_consolidation_due(ws.path())
            .expect("consolidation-due marker queued at the boundary");
        assert_eq!(marker.boundary, "shutdown");
        // AC3: the purge audit notice was queued (never silent).
        let notice = session_queue::read_purge_notice(ws.path())
            .expect("MEMORY.md purge audit notice queued");
        assert_eq!(notice.purged_count, 2);
    }

    /// Story 12.1c AC4 (Task 1) — `on_session_end` is invoked exactly once per
    /// boundary with an EMPTY transcript; the `NoopRecallProvider` default neither
    /// panics nor has side effects, and does NOT short-circuit on empty input.
    #[tokio::test]
    async fn session_end_invokes_recall_provider_once_per_boundary() {
        let ws = tempfile::tempdir().unwrap();
        let mem: Arc<dyn MemoryPort> = Arc::new(RecordingMemory::default());

        // Recording stub ⇒ called exactly once, transcript empty.
        let recall_rec = Arc::new(RecordingRecall::default());
        let recall: Arc<dyn RecallProviderPort> = recall_rec.clone();
        emit_session_boundary(SessionBoundary::Shutdown, &mem, &recall, ws.path(), &[]).await;
        assert_eq!(recall_rec.calls.load(Ordering::SeqCst), 1);
        assert_eq!(recall_rec.max_seen_len.load(Ordering::SeqCst), 0);

        // Noop default ⇒ no panic, zero side-effects (reaching the assert is the pass).
        let noop: Arc<dyn RecallProviderPort> = Arc::new(crate::adapters::noop::NoopRecallProvider);
        emit_session_boundary(SessionBoundary::DailyReset, &mem, &noop, ws.path(), &[]).await;
    }

    /// Review patch — `NoopRecallProvider` must tolerate a non-empty transcript
    /// without short-circuiting or panicking (the trait contract explicitly forbids
    /// baking in `if empty { return }`).
    #[tokio::test]
    async fn noop_recall_provider_tolerates_non_empty_transcript() {
        let noop = crate::adapters::noop::NoopRecallProvider;
        let msg = ChatMessage {
            id: "test-msg".into(),
            role: crate::domain::models::MessageRole::User,
            content: "hello".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin: crate::domain::models::ChannelKind::Terminal,
            authorship: Default::default(),
            retracted_at_ms: None,
        };
        let result = noop.on_session_end(&[msg]).await;
        assert!(
            result.is_ok(),
            "NoopRecallProvider must not panic on non-empty transcript"
        );
    }

    /// Story 12.1c AC2 (Task 5) — the durable consolidation-due marker survives a
    /// runtime teardown + reconstruct (fire all three triggers, drop everything,
    /// re-read from the same path). Latest-only, so the marker replays. No consumer
    /// is asserted — correct, there is none headless (12.2 attach consumes it).
    #[tokio::test]
    async fn boundary_enqueues_durable_proposal_survives_restart() {
        let ws = tempfile::tempdir().unwrap();
        {
            let mem: Arc<dyn MemoryPort> = Arc::new(RecordingMemory::default());
            let recall: Arc<dyn RecallProviderPort> = Arc::new(RecordingRecall::default());
            emit_session_boundary(SessionBoundary::DailyReset, &mem, &recall, ws.path(), &[]).await;
            emit_session_boundary(SessionBoundary::IdleTimeout, &mem, &recall, ws.path(), &[])
                .await;
            emit_session_boundary(SessionBoundary::Shutdown, &mem, &recall, ws.path(), &[]).await;
            // Drop the memory/recall/runtime — only the on-disk queue remains.
        }
        let replayed = session_queue::read_consolidation_due(ws.path())
            .expect("the consolidation-due marker replays from disk after teardown");
        assert_eq!(
            replayed.boundary, "shutdown",
            "latest-only: last boundary wins"
        );
        assert!(
            !replayed.daily_log_ref.is_empty(),
            "marker references a daily-log slice for 12.2 to consolidate"
        );
    }

    #[test]
    fn duration_until_next_handles_same_day_and_wraparound() {
        use chrono::NaiveTime;
        let now = NaiveTime::from_hms_opt(10, 0, 0).unwrap();
        // Later today → that many seconds away.
        let later = NaiveTime::from_hms_opt(10, 0, 30).unwrap();
        assert_eq!(duration_until_next(now, later), Duration::from_secs(30));
        // Earlier than now → tomorrow (wrap +24h).
        let earlier = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        assert_eq!(
            duration_until_next(now, earlier),
            Duration::from_secs(23 * 3600)
        );
        // Exactly now → next day, not zero (don't fire instantly in a loop).
        assert_eq!(
            duration_until_next(now, now),
            Duration::from_secs(24 * 3600)
        );
    }
}
