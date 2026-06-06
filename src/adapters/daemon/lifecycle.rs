//! Daemon lifecycle runtime (Story 12.1a Task 3/4/6) — the headless `tokio::select!`
//! loop, the single `SessionBoundary` emit seam (AC-12-1a-7), and the graceful
//! shutdown path (AC-12-1a-3, NFR48).
//!
//! This loop is **new and minimal**. It is to the daemon what `event_loop::run`
//! is to the TUI, but ~1/100th the size — and crucially it is NOT `event_loop::run`,
//! which is TUI-coupled (`&mut Tui` + `crossterm::EventStream`) and cannot run
//! headless (see Story 12.1a Dev Notes §"Headless seam").

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{SignalKind, signal};

use crate::domain::models::{AppConfig, SessionBoundary};
use crate::domain::ports::MemoryPort;

use super::{pidfile, socket};

/// Everything the lifecycle loop needs. Constructed by `mod::run_daemon_foreground`
/// after composition + socket bind + PID-file write.
pub struct DaemonRuntime {
    pub config: AppConfig,
    pub memory: Arc<dyn MemoryPort>,
    pub pid_path: PathBuf,
    pub socket_path: PathBuf,
}

/// THE single shared emit seam — AC-12-1a-7 "one code path, three triggers".
///
/// All of { `daily_reset` fires, `idle_timeout` fires, graceful shutdown begins }
/// call exactly this function; there are no divergent boundary paths. 12.1a's own
/// boundary action is limited to **finalize the daily log** (drain + flush) +
/// (the caller) reset conversation context + rearm timers.
///
/// The daily-log finalize routes through Story 12.0's **hardened single sink** —
/// `MemoryPort::prepare_detach()` (drain-await-quiescence) then `flush()`. We do
/// NOT open a parallel purge/finalize path (12.0 AC9: "harden the sink, not the
/// call-sites"), so 12.4 cron and the future file-edit path stay auto-covered.
pub async fn emit_session_boundary(boundary: SessionBoundary, memory: &Arc<dyn MemoryPort>) {
    tracing::info!(boundary = %boundary, "daemon SessionBoundary");

    // Finalize the daily log via the 12.0 hardened sink (drain-await-quiescence
    // then flush). In 12.1a there is no live message source, so this is usually a
    // no-op drain — but routing through the same sink is what keeps the seam
    // correct once 12.2+ feed messages.
    if let Err(e) = memory.prepare_detach().await {
        tracing::warn!(boundary = %boundary, error = %e, "prepare_detach at boundary failed");
    }
    if let Err(e) = memory.flush().await {
        tracing::warn!(boundary = %boundary, error = %e, "memory flush at boundary failed");
    }

    // ── Story 12.1c hooks here (NO-OP in 12.1a — do not implement) ────────────
    // 12.1c will hang the following off THIS single seam (never a parallel path):
    //   • RecallProviderPort::on_session_end(transcript)  (declared per ADR-11-1)
    //   • the 11.2a propose→confirm session-end consolidation card
    //   • the MEMORY.md redaction-honor path (funnels through 12.0's refresh sink)
    // 12.1a deliberately provides only the emit point + this extension seam.
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
pub async fn run_lifecycle(rt: DaemonRuntime) -> Result<()> {
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

    // Timers. The idle timer arm is disabled (via the `if !low_power` guard) once
    // we are already in low-power, and re-armed on activity.
    let mut daily_timer = Box::pin(tokio::time::sleep(next_daily_reset(daily_target)));
    let mut idle_timer = Box::pin(tokio::time::sleep(idle));
    let mut low_power = false;

    tracing::info!(
        idle_secs = idle.as_secs(),
        daily_reset = %rt.config.daemon.daily_reset,
        "daemon lifecycle ready (headless; no message source until Story 12.2)"
    );

    loop {
        tokio::select! {
            _ = sigterm.recv() => { tracing::info!("daemon received SIGTERM"); break; }
            _ = sigint.recv()  => { tracing::info!("daemon received SIGINT");  break; }
            _ = sighup.recv()  => { tracing::info!("daemon received SIGHUP");  break; }

            _ = &mut daily_timer => {
                if tokio::time::timeout(
                    Duration::from_secs(2),
                    emit_session_boundary(SessionBoundary::DailyReset, &rt.memory),
                )
                .await
                .is_err()
                {
                    tracing::warn!("daemon: daily_reset boundary emit timed out (>2s)");
                }
                // 12.1a boundary action: reset conversation context is a no-op
                // (no conversation yet) + rearm both timers. A daily reset counts
                // as activity, so leave low-power.
                low_power = false;
                daily_timer = Box::pin(tokio::time::sleep(next_daily_reset(daily_target)));
                idle_timer = Box::pin(tokio::time::sleep(idle));
            }

            _ = &mut idle_timer, if !low_power => {
                low_power = true;
                tracing::info!("daemon entered low-power state");
                if low_power_emits {
                    if tokio::time::timeout(
                        Duration::from_secs(2),
                        emit_session_boundary(SessionBoundary::IdleTimeout, &rt.memory),
                    )
                    .await
                    .is_err()
                    {
                        tracing::warn!("daemon: idle_timeout boundary emit timed out (>2s)");
                    }
                }
                // Stay in low-power until activity reactivates us (the arm is now
                // guarded off, so the elapsed timer won't busy-loop).
            }

            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        // 12.1a: log + close stub. The attach wire protocol is 12.2.
                        drop(stream);
                        tracing::info!("daemon socket: accepted + closed (attach protocol = Story 12.2)");
                    }
                    Err(e) => tracing::warn!(error = %e, "daemon socket accept failed"),
                }
                // Activity → reactivate from low-power + reset the idle timer.
                if low_power {
                    low_power = false;
                    tracing::info!("daemon reactivated from low-power");
                }
                idle_timer = Box::pin(tokio::time::sleep(idle));
            }
        }
    }

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
    if tokio::time::timeout(
        Duration::from_secs(3),
        emit_session_boundary(SessionBoundary::Shutdown, &rt.memory),
    )
    .await
    .is_err()
    {
        tracing::warn!("daemon shutdown: boundary flush timed out (>3s), proceeding to exit");
    }

    // Channels (`ChannelPort::shutdown_loop`) and MCP-client teardown are not
    // composed in 12.1a (no message runtime); they enter this same path in 12.2+.

    socket::cleanup(&rt.socket_path);
    pidfile::remove(&rt.pid_path);
    tracing::info!("daemon graceful shutdown complete");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::domain::errors::{MemoryError, TransitionError};
    use crate::domain::models::TransitionState;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Recording memory adapter — counts the sink calls so we can prove all three
    /// triggers reach ONE shared path (AC-12-1a-7), deterministically, no sleep.
    /// Only `flush`/`prepare_detach` are overridden; the rest keep trait defaults.
    #[derive(Default)]
    struct RecordingMemory {
        prepare_detach_calls: AtomicUsize,
        flush_calls: AtomicUsize,
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
    }

    #[tokio::test]
    async fn all_three_triggers_route_through_one_sink() {
        let rec = Arc::new(RecordingMemory::default());
        let mem: Arc<dyn MemoryPort> = rec.clone();
        emit_session_boundary(SessionBoundary::DailyReset, &mem).await;
        emit_session_boundary(SessionBoundary::IdleTimeout, &mem).await;
        emit_session_boundary(SessionBoundary::Shutdown, &mem).await;

        // Each of the three boundaries drove the SAME sink exactly once → one path.
        assert_eq!(rec.prepare_detach_calls.load(Ordering::SeqCst), 3);
        assert_eq!(rec.flush_calls.load(Ordering::SeqCst), 3);
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
