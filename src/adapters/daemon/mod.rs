//! Daemon adapter (Story 12.1a) — `rustain daemon {start,stop,status}` + the
//! hidden `__run` re-exec body.
//!
//! **Scope (read Dev Notes §"Scope discipline"):** 12.1a is the daemon PROCESS +
//! LIFECYCLE skeleton — start/stop/status, PID file, socket bind, the
//! `daily_reset`/`idle_timeout`/shutdown boundaries, and graceful shutdown. It is
//! NOT a message-processing runtime: `event_loop::run` is TUI-coupled and cannot
//! run headless, and there is no live channel yet (`terminal` → `NoOpChannel`).
//! Message delivery lands in Stories 12.2/12.3/12.4.
//!
//! This adapter owns OS I/O (Unix socket, PID file, process spawn) → it sits in
//! the **adapters** layer per the Hexagonal map. It is Unix-only (Linux P0,
//! macOS P1); on Windows every entrypoint returns an actionable not-supported
//! error (named-pipe support deferred to P2 / NFR33).

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::adapters::cli::commands::DaemonAction;
use crate::domain::models::AppConfig;

#[cfg(unix)]
mod attach_client;
#[cfg(unix)]
pub(crate) mod consent;
#[cfg(unix)]
mod crash;
#[cfg(unix)]
mod lifecycle;
#[cfg(unix)]
mod pidfile;
#[cfg(unix)]
mod policy_startup;
#[cfg(unix)]
mod procargs;
#[cfg(unix)]
pub mod protocol;
#[cfg(unix)]
pub(crate) mod response_modes;
#[cfg(unix)]
pub mod runtime;
#[cfg(unix)]
pub mod server;
#[cfg(unix)]
mod service;
#[cfg(unix)]
pub mod session_holder;
#[cfg(unix)]
mod session_queue;
#[cfg(unix)]
mod socket;
#[cfg(unix)]
pub mod status;
#[cfg(unix)]
pub(crate) mod urgency;

#[cfg(unix)]
pub use lifecycle::{duration_until_next, emit_session_boundary};

/// Dispatch a `daemon` subcommand. `memory_adapter` is the active profile's
/// resolved memory port name (used only by the `__run`/foreground body to compose
/// the headless memory sink); `config` carries `[daemon]` settings + profile.
pub async fn run_daemon(
    action: DaemonAction,
    workspace: PathBuf,
    config: AppConfig,
    memory_adapter: String,
    selection: crate::domain::models::profile::ProfileSelection,
    a2a_peers: Vec<crate::domain::models::A2aPeerSpec>,
    // Story 18.1b — `--serve-a2a=ADDR` combined with daemon mode. The A2A
    // listener runs as a sibling `tokio::spawn` inside this daemon's lifecycle,
    // sharing its `NodeTree`, `DaemonCore` and event bus. There is no second
    // core.
    serve_a2a: Option<String>,
    p2p_listen: bool,
) -> Result<()> {
    #[cfg(unix)]
    {
        match action {
            DaemonAction::Start { foreground } => {
                if foreground {
                    run_daemon_foreground(
                        workspace,
                        config,
                        memory_adapter,
                        selection,
                        a2a_peers,
                        serve_a2a,
                        p2p_listen,
                    )
                    .await
                } else {
                    run_daemon_start(workspace, config, serve_a2a).await
                }
            }
            DaemonAction::Run => {
                run_daemon_foreground(
                    workspace,
                    config,
                    memory_adapter,
                    selection,
                    a2a_peers,
                    serve_a2a,
                    p2p_listen,
                )
                .await
            }
            DaemonAction::Stop => run_daemon_stop(workspace).await,
            // Story 12.2c — default to the rich multi-channel TUI; `--plain` keeps
            // the line-based 12.2b client for scripting/non-TTY use.
            DaemonAction::Attach { plain } => {
                if plain {
                    attach_client::run_attach(&workspace).await
                } else {
                    crate::infrastructure::runtime::attach_loop::run_attached(&workspace).await
                }
            }
            DaemonAction::Status { json } => run_daemon_status(workspace, config, json).await,
            // install/uninstall are pure generate/remove — no memory composition, no
            // async I/O (AC-12-1b-3/3b). Called synchronously inside the async fn.
            DaemonAction::Install { print, system } => {
                run_daemon_install(workspace, config, print, system)
            }
            DaemonAction::Uninstall { system } => run_daemon_uninstall(workspace, system),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (
            action,
            workspace,
            config,
            memory_adapter,
            selection,
            serve_a2a,
            p2p_listen,
        );
        windows_not_supported()
    }
}

#[cfg(not(unix))]
fn windows_not_supported() -> Result<()> {
    eprintln!(
        "Error: `rustain daemon` requires Unix sockets (Linux or macOS). \
         Windows daemon support (named pipes) is deferred to a future release (NFR33). \
         Run rustain in interactive (TUI) mode instead."
    );
    anyhow::bail!("daemon mode is not supported on this platform")
}

// ── Unix implementation ──────────────────────────────────────────────────────

#[cfg(unix)]
use lifecycle::DaemonRuntime;
#[cfg(unix)]
use pidfile::{DaemonPidFile, GuardOutcome};

/// `daemon start` — re-exec a detached child (NOT `fork()`; forking a live
/// multi-threaded tokio runtime is unsafe — only async-signal-safe calls are
/// legal between fork and exec). The parent waits for the readiness handshake
/// (the child writing its PID file) within the NFR47 3s budget, then returns.
#[cfg(unix)]
async fn run_daemon_start(
    workspace: PathBuf,
    config: AppConfig,
    serve_a2a: Option<String>,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // Validate `[daemon]` config here so a bad value fails the foreground `start`
    // command rather than silently dying in the detached child (Task 7).
    config.daemon.validate().map_err(|e| anyhow::anyhow!(e))?;

    let pid_path = crate::infrastructure::paths::daemon_pid_path(&workspace)?;

    // Already-running guard (AC-12-1a-9) — exact message, stale reclaim.
    match pidfile::check_running(&pid_path) {
        GuardOutcome::Running(pf) => {
            eprintln!(
                "Daemon already running (PID: {}). Use 'rustain daemon stop' first.",
                pf.pid
            );
            anyhow::bail!("daemon already running");
        }
        GuardOutcome::Stale => {
            // A leftover PID file whose process is gone is an unclean exit
            // (AC-12-1b-4). Do NOT reclaim/record here: leave it for the re-exec'd
            // foreground child, which owns the SINGLE crash-detection path so the
            // recovery line lands in the daemon log (the child's stdout). The
            // readiness poll below waits for the child to overwrite this stale file
            // with its own PID, so leaving it is safe.
            tracing::info!(
                "found stale daemon PID file at {}; the daemon will record + reclaim it",
                pid_path.display()
            );
        }
        GuardOutcome::Free => {}
    }

    let exe = std::env::current_exe()?;
    let log_path = crate::infrastructure::paths::daemon_log_path(&workspace)?;
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)?;
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(exe);
    // Forward the resolved profile so the child composes the SAME memory adapter.
    cmd.arg("--profile").arg(&config.active_profile);
    // Story 18.1b — the detached child is the process that actually serves, so
    // the flag has to travel with it. `--serve-a2a` uses `require_equals`, so it
    // must be passed as one `=`-joined argument.
    if let Some(addr) = serve_a2a.as_deref() {
        cmd.arg(format!("--serve-a2a={addr}"));
    }
    cmd.arg("daemon").arg("__run");
    cmd.current_dir(&workspace);
    // Lineage nonce injection (Story 12.1c P1): generate the nonce HERE and pass it to
    // the child via the environment so the live daemon *carries* it (observable via
    // `/proc/<pid>/environ`). This makes the nonce load-bearing for ownership — a
    // recycled foreign PID won't echo it, so `stop`/the guard won't mistake it for
    // ours (D-1). The child writes this same nonce into its PID file.
    cmd.env(pidfile::DAEMON_NONCE_ENV, pidfile::generate_nonce());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log));
    cmd.stderr(Stdio::from(log_err));
    // Detach from the controlling terminal: new session via setsid in the child,
    // after fork() but before exec(). setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            // SAFETY: setsid takes no args, touches no process memory, and is
            // async-signal-safe — legal in the post-fork/pre-exec window.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    let child_pid = child.id();

    // Readiness handshake: poll for the child's PID file (written last, just
    // before the loop) within 3s (NFR47). Bail early if the child dies.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(pf) = DaemonPidFile::read(&pid_path) {
            if pf.pid == child_pid {
                println!("Daemon started (PID: {child_pid}).");
                return Ok(());
            }
        }
        if let Ok(Some(exit)) = child.try_wait() {
            anyhow::bail!(
                "daemon child exited before becoming ready ({exit}); see {}",
                log_path.display()
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            anyhow::bail!(
                "daemon did not become ready within 3s; see {}",
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The daemon body (the `__run` re-exec target and `start --foreground`): compose
/// the headless memory sink, bind the socket, write the PID file (the readiness
/// marker), and run the lifecycle loop until a shutdown signal.
#[cfg(unix)]
async fn run_daemon_foreground(
    workspace: PathBuf,
    config: AppConfig,
    memory_adapter: String,
    selection: crate::domain::models::profile::ProfileSelection,
    a2a_peers: Vec<crate::domain::models::A2aPeerSpec>,
    serve_a2a: Option<String>,
    p2p_listen: bool,
) -> Result<()> {
    config.daemon.validate().map_err(|e| anyhow::anyhow!(e))?;

    // One read supplies both replay-folded consent and restart-safe digest
    // state. Neither projection re-reads the journal on delivery.
    let journal_reader =
        crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
            &workspace,
        );
    let journal_entries = crate::domain::ports::RoomJournalReader::load_entries(&journal_reader)
        .await
        .context("failed to load policy and urgency journal projections")?;
    let consent_projection = std::sync::Arc::new(
        crate::adapters::policy::JournalConsentProjection::from_entries(&journal_entries),
    );
    let effective_policy = policy_startup::validate_startup_policies(
        &workspace,
        &a2a_peers,
        consent_projection.as_ref(),
    )?;
    let effective_policy = std::sync::Arc::new(effective_policy);
    // The admission value only feeds the authority-widening WARNING — a
    // malformed optional-listener config must not be fatal to daemon startup
    // (previously it could only break the feature-gated A2A listener itself).
    match crate::adapters::a2a::config::parse_workspace_a2a_server_config(
        &crate::infrastructure::paths::workspace_a2a_config_path(&workspace),
    ) {
        Ok(config) => {
            let admission = config.map_or_else(Default::default, |server| server.admission);
            policy_startup::report_auto_authority_widening(
                admission,
                &effective_policy,
                &a2a_peers,
                consent_projection.as_ref(),
            );
        }
        Err(error) => policy_startup::report_unknown_admission_posture(&error),
    }

    let pid_path = crate::infrastructure::paths::daemon_pid_path(&workspace)?;
    let socket_path = crate::infrastructure::paths::daemon_socket_path(&workspace)?;

    // Defense-in-depth guard inside the child too (the parent already checked, but a
    // foreground invocation — the supervised systemd/launchd entrypoint — skips the
    // parent path entirely). This is THE crash-detection seam (AC-12-1b-4): when the
    // supervisor relaunches us after an unclean exit, the leftover PID file shows up
    // here as `Stale`.
    let singleton = crate::infrastructure::subagent::DaemonSingletonLock::try_acquire(&workspace)
        .await
        .map_err(|error| anyhow::anyhow!("acquiring daemon singleton: {error}"))?;
    match pidfile::check_running(&pid_path) {
        GuardOutcome::Running(pf) => {
            eprintln!(
                "Daemon already running (PID: {}). Use 'rustain daemon stop' first.",
                pf.pid
            );
            anyhow::bail!("daemon already running");
        }
        GuardOutcome::Stale => {
            // Unclean prior exit → record + announce, then reclaim and start normally
            // (MUST NOT refuse to start / require manual cleanup — AC-12-1b-4).
            crash::detect_and_record_stale(&workspace, &pid_path);
            pidfile::remove(&pid_path);
        }
        // Clean start (no pre-existing PID file) records no crash event.
        GuardOutcome::Free => {}
    }

    // Story 12.2b — compose the daemon CORE: eager memory/storage/security/persona
    // + a lazy `TurnRuntimeFactory` behind a `OnceCell` (idle holds no live
    // provider — NFR46). `build_daemon_core` reuses the same `build_*` factories
    // startup uses (no forked composition). The per-activation event bus is created
    // here so the factory can capture its `domain_tx` and the forwarder owns the rx.
    let config_swap = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(config.clone()));
    let (event_bus, domain_rx) = crate::infrastructure::runtime::event_bus::EventBus::new(
        config.runtime.event_bus.raw_capacity.max(1),
    );
    let domain_tx = event_bus.domain_tx.clone();
    let (channel_turn_tx, channel_turn_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::domain::models::ChannelTurnRequest>();
    let node_journal = std::sync::Arc::new(
        crate::infrastructure::subagent::NodeJournal::open_workspace(&workspace)
            .await
            .map_err(|error| anyhow::anyhow!("opening node journal: {error}"))?,
    );
    let clock: std::sync::Arc<dyn crate::domain::clock::Clock> =
        std::sync::Arc::new(crate::domain::clock::SystemClock::default());
    let now_fn = {
        let clock = clock.clone();
        std::sync::Arc::new(move || clock.wall_now_ms())
    };
    let node_tree =
        crate::infrastructure::subagent::NodeTree::with_event_tx(domain_tx.clone(), now_fn)
            .with_journal(node_journal.clone())
            .with_host_binding(crate::infrastructure::subagent::current_host_binding(
                &workspace,
            ));
    let recovery = crate::infrastructure::subagent::NodeRecovery::reconcile(
        &node_journal,
        &node_tree,
        &singleton,
        &crate::infrastructure::subagent::current_host_id(&workspace),
    )
    .await
    .map_err(|error| anyhow::anyhow!("recovering durable nodes: {error}"))?;
    tracing::info!(
        restored = recovery.restored.len(),
        suspended = recovery.suspended.len(),
        failed = recovery.failed.len(),
        "durable node recovery complete"
    );
    // Periodically escalate `Waiting` nodes whose persisted wall-clock dwell
    // crosses the hazard threshold. The dwell rides the injected clock; this
    // interval is only the polling cadence. The 17.2b supervisor will own the
    // richer scheduling and consume the journaled hazard markers.
    {
        let hazard_tree = node_tree.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let _ = hazard_tree
                    .raise_due_hazards(crate::domain::models::WAITING_HAZARD_THRESHOLD_MS)
                    .await;
            }
        });
    }
    #[cfg(feature = "a2a")]
    let a2a_egress = std::sync::Arc::new(
        crate::adapters::a2a::egress::A2aEgress::compose(
            a2a_peers.clone(),
            node_tree.clone(),
            std::sync::Arc::new(
                crate::infrastructure::subagent::node_journal::NodeRoomJournal::new(
                    node_journal.clone(),
                    Some(domain_tx.clone()),
                ),
            ),
            node_journal.clone(),
            domain_tx.clone(),
        )
        .map_err(|error| anyhow::anyhow!("composing daemon A2A egress: {error}"))?,
    );

    let core = std::sync::Arc::new(
        crate::infrastructure::composition::build_daemon_core(
            &workspace,
            config_swap,
            selection.clone(),
            &memory_adapter,
            domain_tx.clone(),
            Some(channel_turn_tx.clone()),
            node_tree.clone(),
            node_journal.clone(),
            #[cfg(feature = "a2a")]
            a2a_egress,
        )
        .map_err(|e| anyhow::anyhow!("composing daemon core: {e}"))?,
    );

    // Story 12.2b AC4 — load/restore the per-process conversation from the
    // workspace session (most-recent), else start fresh + persist. Re-attach
    // (12.2c) and the boundary loop see this same transcript.
    let conversation = std::sync::Arc::new(tokio::sync::Mutex::new(
        load_or_new_conversation(core.storage.as_ref()).await,
    ));

    // The recall provider stays the offline no-op (a real backend arrives with
    // Story 11.5); the seam is still DRIVEN every boundary with the REAL transcript.
    let recall: std::sync::Arc<dyn crate::domain::ports::RecallProviderPort> =
        std::sync::Arc::new(crate::adapters::noop::NoopRecallProvider);

    let channel: std::sync::Arc<dyn crate::domain::ports::ChannelPort> = {
        let chan_name = selection
            .dimensions
            .get(&crate::domain::models::PortDimension::Channels)
            .map(|a| a.adapter.as_str())
            .unwrap_or("terminal");
        let chan_config = selection
            .dimensions
            .get(&crate::domain::models::PortDimension::Channels)
            .and_then(|a| a._config.as_ref());
        let chan_ctx = crate::infrastructure::composition::daemon_compose_context(
            &workspace,
            core.storage.clone(),
            domain_tx.clone(),
            config.assembler.strategy.clone(),
            Some(channel_turn_tx),
            core.peer_topic_store.clone(),
        );
        crate::infrastructure::composition::build_channels(chan_name, chan_config, &chan_ctx)
            .unwrap_or_else(|e| {
                tracing::warn!(adapter = chan_name, error = %e, "daemon: channel adapter composition failed; using terminal noop channel");
                std::sync::Arc::new(crate::adapters::noop::NoOpChannel)
            })
    };

    #[cfg(feature = "cron")]
    let (cron_completion_tx, cron_completion_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::adapters::scheduler::cron::CronCompletion>();

    let scheduler: std::sync::Arc<dyn crate::domain::ports::SchedulerPort> = {
        let sched_name = selection
            .dimensions
            .get(&crate::domain::models::PortDimension::Scheduler)
            .map(|a| a.adapter.as_str())
            .unwrap_or("none");
        #[cfg(feature = "cron")]
        {
            if sched_name == "cron" {
                let cron_path = crate::adapters::scheduler::cron::cron_toml_path()
                    .unwrap_or_else(|_| std::path::PathBuf::from("cron.toml"));
                match crate::adapters::scheduler::cron::CronSchedulerAdapter::load(
                    cron_path,
                    core.clone(),
                    cron_completion_tx,
                    channel.clone(),
                    core.storage.clone(),
                )
                .await
                {
                    Ok(adapter) => std::sync::Arc::new(adapter)
                        as std::sync::Arc<dyn crate::domain::ports::SchedulerPort>,
                    Err(e) => {
                        tracing::warn!(adapter = sched_name, error = %e, "daemon: cron scheduler composition failed; using noop scheduler");
                        std::sync::Arc::new(crate::adapters::noop::NoOpScheduler)
                    }
                }
            } else {
                std::sync::Arc::new(crate::adapters::noop::NoOpScheduler)
            }
        }
        #[cfg(not(feature = "cron"))]
        {
            let _ = sched_name;
            std::sync::Arc::new(crate::adapters::noop::NoOpScheduler)
        }
    };

    // The room journal is shared by peer transparency and the same-host
    // response action dispatcher. Both write through the same durable port.
    let reader: std::sync::Arc<dyn crate::domain::ports::RoomJournalReader> = node_journal.clone();
    let journal: std::sync::Arc<dyn crate::domain::ports::RoomJournal> = std::sync::Arc::new(
        crate::infrastructure::subagent::node_journal::NodeRoomJournal::new(
            node_journal,
            Some(domain_tx.clone()),
        ),
    );
    let urgency_router = std::sync::Arc::new(crate::adapters::daemon::urgency::UrgencyRouter::new(
        clock,
        journal.clone(),
        &journal_entries,
        i64::from(effective_policy.digest_interval_minutes).saturating_mul(60_000),
    ));

    // Story 18.3c (AC1) — the composition root installs the already-resolved
    // effective policy into the ONE bus consumed by verified peer delivery.
    // Relationship disposition remains a separate decision inside the policy.
    let delivery_policy: std::sync::Arc<dyn crate::domain::ports::DeliveryPolicy> =
        std::sync::Arc::new(crate::domain::ports::EffectiveDeliveryPolicy::new(
            effective_policy.clone(),
        ));
    let peer_bus = crate::adapters::daemon::server::peer_bus_slot_with_policy(
        &node_tree,
        delivery_policy.clone(),
    );
    let server =
        crate::adapters::daemon::server::AttachServer::new_with_node_tree_bus_policy_journal_and_urgency(
            core.clone(),
            conversation.clone(),
            domain_tx.clone(),
            node_tree,
            peer_bus,
            delivery_policy,
            journal.clone(),
            reader.clone(),
            Some(consent_projection.clone()),
            Some(urgency_router.clone()),
        );
    server
        .configure_consent_policy(effective_policy.clone())
        .await;
    if let Some(batch) = urgency_router
        .flush_pending_on_start()
        .await
        .context("failed to prepare pending startup digest")?
    {
        server
            .surface_digest_batch(batch.clone())
            .await
            .context("failed to surface pending startup digest")?;
        urgency_router
            .commit_flush(&batch)
            .await
            .context("failed to journal pending startup digest")?;
    }
    let urgency_shutdown = tokio_util::sync::CancellationToken::new();
    let urgency_task = tokio::spawn(crate::adapters::daemon::urgency::run_digest_flusher(
        urgency_router,
        std::sync::Arc::downgrade(&server),
        urgency_shutdown.child_token(),
    ));

    // The recorder is mandatory for every live verified peer frame, regardless
    // of whether the optional HTTP A2A listener is enabled.
    let notices: std::sync::Arc<dyn crate::domain::ports::EventEmitter> = std::sync::Arc::new(
        crate::infrastructure::runtime::event_bus::ChannelEmitter::new(domain_tx.clone()),
    );
    let transparency = std::sync::Arc::new(
        crate::adapters::a2a::transparency::TransparencySink::new(journal)
            .with_reader(reader)
            .with_notices(notices),
    );
    server
        .configure_peer_recorder(
            transparency.clone()
                as std::sync::Arc<dyn crate::domain::ports::PeerInteractionRecorder>,
            core.peer_topic_store.clone(),
        )
        .await;
    arm_node_recovery_harness(&server).await?;

    // Story 18.1b, AC5b — the A2A listener is a SIBLING `tokio::spawn` inside
    // this daemon's lifecycle. It shares this `node_tree`, this
    // `Arc<DaemonCore>` and this `domain_tx` through `AttachServer` (which is
    // the `InboundPeerRuntime`), so an inbound A2A task runs on exactly the peer-turn
    // path the Unix socket drives. There is no second core and no second tree.
    let a2a_shutdown = tokio_util::sync::CancellationToken::new();
    let a2a_task = spawn_a2a_listener(
        serve_a2a,
        &workspace,
        &config,
        server.clone(),
        transparency,
        a2a_shutdown.child_token(),
    )
    .await?;

    let p2p_shutdown = tokio_util::sync::CancellationToken::new();
    let p2p_task = spawn_p2p_listener(
        p2p_listen,
        &workspace,
        server.clone(),
        p2p_shutdown.child_token(),
    )
    .await?;

    let rt = DaemonRuntime {
        config: config.clone(),
        memory: core.memory.clone(),
        recall,
        workspace: workspace.clone(),
        pid_path: pid_path.clone(),
        socket_path: socket_path.clone(),
        server,
        channel,
        scheduler,
        domain_rx: Some(domain_rx),
        channel_turn_rx: Some(channel_turn_rx),
        #[cfg(feature = "cron")]
        cron_completion_rx: Some(cron_completion_rx),
        conversation,
    };

    // Write the PID file LAST (after we know paths resolve) — it is the readiness
    // marker the parent `start` polls for. Records socket + workspace + start time
    // (AC-12-1a-8) so status/stop/attach read rather than re-derive.
    let started_at_unix = now_unix();
    let pid = std::process::id();
    let pf = DaemonPidFile {
        pid,
        socket_path,
        workspace: workspace.clone(),
        started_at_unix,
        profile: config.active_profile.clone(),
        // Lineage hardening (Story 12.1b AC-12-1b-8, 12.1c P1): use the nonce the
        // parent `start` injected via env (so the PID-file nonce == the nonce this
        // process carries in its environment, making ownership verifiable); fall back
        // to a fresh nonce when started directly (systemd/launchd/`--foreground`),
        // where ownership instead rests on the exact-comm + argv-token fallback.
        nonce: crate::infrastructure::utils::env_var_trimmed(pidfile::DAEMON_NONCE_ENV)
            .unwrap_or_else(pidfile::generate_nonce),
        boot_id: pidfile::current_boot_id(),
    };
    pf.write_atomic(&pid_path)?;

    // Headless daemon panic hook (AC-12-1b-5) — installed AFTER composition + PID
    // write so it carries full daemon context (pid/profile/workspace/started). It
    // writes a `reason: "panic: …"` crash record + a capped backtrace file WITHOUT
    // terminal assumptions, then chains to the prior (global TUI) hook. The stale-PID
    // detector above is the PRIMARY signal (catches SIGKILL/OOM, which no hook can);
    // this hook is best-effort backtrace enrichment for the panic death mode.
    crash::install_daemon_panic_hook(crash::DaemonPanicContext {
        pid,
        profile: config.active_profile.clone(),
        workspace: workspace.clone(),
        started_at_unix,
    });

    let result = lifecycle::run_lifecycle(rt).await;

    // Signal every listener FIRST, then await them. Cancelling in the same pass
    // that awaits lets an earlier listener's five-second grace run while a later
    // one is still admitting remote frames into a daemon whose lifecycle has
    // already stopped.
    urgency_shutdown.cancel();
    a2a_shutdown.cancel();
    p2p_shutdown.cancel();

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), urgency_task).await;

    if let Some(task) = a2a_task {
        // Bounded: a listener that will not stop must not hold the daemon's exit
        // hostage — the process is going away regardless.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    if let Some(task) = p2p_task {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    // Belt-and-suspenders: ensure the PID file is gone even if the loop errored
    // before its own cleanup ran.
    pidfile::remove(&pid_path);
    drop(singleton);
    result
}

/// Await the A2A listener's bind/configuration handshake before publishing the
/// daemon's PID readiness marker.
#[cfg(all(unix, feature = "a2a"))]
async fn wait_for_a2a_listener_ready(
    ready: tokio::sync::oneshot::Receiver<std::result::Result<(), String>>,
) -> Result<()> {
    match ready.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => anyhow::bail!("A2A listener failed to start: {error}"),
        Err(error) => {
            anyhow::bail!("A2A listener exited before reporting readiness: {error}")
        }
    }
}

#[cfg(all(test, unix, feature = "a2a"))]
mod a2a_listener_readiness_tests {
    use super::wait_for_a2a_listener_ready;

    #[tokio::test]
    async fn listener_startup_errors_block_daemon_readiness() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        ready_tx
            .send(Err("TLS configuration is invalid".to_owned()))
            .expect("receiver remains available");

        let error = wait_for_a2a_listener_ready(ready_rx)
            .await
            .expect_err("listener startup error must fail daemon startup");

        assert_eq!(
            error.to_string(),
            "A2A listener failed to start: TLS configuration is invalid"
        );
    }

    #[tokio::test]
    async fn dropped_listener_readiness_blocks_daemon_readiness() {
        let (ready_tx, ready_rx) =
            tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
        drop(ready_tx);

        let error = wait_for_a2a_listener_ready(ready_rx)
            .await
            .expect_err("a dropped readiness sender must fail daemon startup");

        assert!(
            error
                .to_string()
                .contains("exited before reporting readiness")
        );
    }
}

/// Compose and spawn the A2A listener alongside the daemon, when `--serve-a2a`
/// asked for it.
///
/// Returns `Ok(None)` when the flag is absent (or the build lacks the `a2a`
/// feature), so the daemon runs exactly as before.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn spawn_a2a_listener(
    addr: Option<String>,
    workspace: &std::path::Path,
    config: &AppConfig,
    server: std::sync::Arc<crate::adapters::daemon::server::AttachServer>,
    transparency: std::sync::Arc<crate::adapters::a2a::transparency::TransparencySink>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let Some(addr) = addr else {
        let _ = (workspace, config, server, transparency, shutdown);
        return Ok(None);
    };
    #[cfg(not(feature = "a2a"))]
    {
        let _ = (workspace, config, server, transparency, shutdown);
        anyhow::bail!(
            "--serve-a2a={addr} was requested but this build has the `a2a` feature disabled"
        );
    }
    #[cfg(feature = "a2a")]
    {
        use crate::domain::ports::InboundPeerRuntime;
        let runtime: std::sync::Arc<dyn InboundPeerRuntime> = server;
        let workspace = workspace.to_path_buf();
        let config = config.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let listener_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                result = crate::adapters::a2a::server::run(
                    addr,
                    config,
                    workspace,
                    Some(runtime),
                    transparency,
                    Some(ready_tx),
                ) => {
                    match result {
                        Ok(()) if listener_shutdown.is_cancelled() => {
                            tracing::debug!("daemon A2A listener stopped during shutdown");
                        }
                        Ok(()) => {
                            tracing::error!("daemon A2A listener stopped unexpectedly");
                        }
                        Err(error) if listener_shutdown.is_cancelled() => {
                            tracing::debug!("daemon A2A listener stopped during shutdown: {error:#}");
                        }
                        Err(error) => {
                            tracing::error!("daemon A2A listener stopped: {error:#}");
                        }
                    }
                }
                () = listener_shutdown.cancelled() => {}
            }
        });

        match wait_for_a2a_listener_ready(ready_rx).await {
            Ok(()) => Ok(Some(task)),
            Err(error) => {
                shutdown.cancel();
                task.abort();
                let _ = task.await;
                Err(error)
            }
        }
    }
}

/// Compose the config-gated QUIC listener beside the daemon's existing peer
/// front door. The disabled path allocates nothing and binds no endpoint.
#[cfg(unix)]
async fn spawn_p2p_listener(
    enabled: bool,
    workspace: &std::path::Path,
    server: std::sync::Arc<crate::adapters::daemon::server::AttachServer>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    if !enabled {
        let _ = (workspace, server, shutdown);
        return Ok(None);
    }
    #[cfg(not(feature = "p2p"))]
    {
        let _ = (workspace, server, shutdown);
        anyhow::bail!(
            ".rustain/p2p.json enables the listener but this build has the `p2p` feature disabled"
        );
    }
    #[cfg(feature = "p2p")]
    {
        let handler = server
            .verified_peer_handler()
            .await
            .context("peer delivery front door is not configured")?;
        let signer =
            crate::adapters::rap::IdentityKeyStore::new(crate::infrastructure::paths::data_dir()?)
                .load_or_generate()
                .context("loading the local peer identity key")?;
        let listener = compose_p2p_listener(
            workspace,
            handler,
            signer.transport_secret_key_bytes(),
            shutdown,
        )
        .await?;
        tracing::info!(
            address = listener.address,
            "{}",
            crate::adapters::cli::peer::rows::listener_reach_line(&listener.relay)
        );
        // AC9 — the disclosure is emitted where the operator reads the ready
        // line too (Story 18.4c review): a host that composes a relay says a
        // third party is carrying its traffic, and the startup log is a surface
        // the completion record names. `None` on a `disabled` host.
        if let Some(disclosure) =
            crate::adapters::cli::peer::rows::relay_disclosure(&listener.relay)
        {
            tracing::info!("{disclosure}");
        }
        Ok(Some(listener.task))
    }
}

/// A bound listener, the address an operator can hand to a peer, and the relay
/// mode it composed — which the ready line has to name, because a `disabled`
/// host and a relay-composed host reach different sets of peers.
#[cfg(all(unix, feature = "p2p"))]
struct P2pListener {
    address: String,
    relay: crate::domain::models::RelayConfigState,
    task: tokio::task::JoinHandle<()>,
}

/// Everything the listener is, minus the two lookups that reach outside this
/// process (the front door on `AttachServer` and the on-disk identity key).
///
/// Split out so the composition itself — bind, allowlist health, ingress, and
/// shutdown — is reachable by a test that delivers a real frame through it,
/// instead of being asserted about by reading this file's source.
#[cfg(all(unix, feature = "p2p"))]
async fn compose_p2p_listener(
    workspace: &std::path::Path,
    handler: std::sync::Arc<crate::adapters::rap::VerifiedPeerFrameHandler>,
    transport_secret_key: [u8; 32],
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<P2pListener> {
    use crate::domain::ports::PeerTransport;

    // ⚑ The relay mode is read **before** the bind and reaches the composition
    // itself, not a log line about it. Absent means `disabled`, which is
    // byte-for-byte the endpoint every shipped build already composes; a
    // malformed file degrades to `disabled` too, and says so distinguishably
    // rather than taking the whole peer transport down over one corrupt byte.
    let relay = crate::adapters::relay_config::load_workspace_relay_config(
        &crate::infrastructure::paths::workspace_relay_config_path(workspace),
    );
    if let Some(reason) = relay.degraded_reason() {
        // The row names the FILE and the REASON. Without it the operator
        // degrades into a mode nobody can tell apart from the one they chose,
        // and never learns their configuration is broken.
        tracing::warn!(
            file = %crate::infrastructure::paths::workspace_relay_config_path(workspace).display(),
            %reason,
            "the relay configuration did not read; this host composed no relay"
        );
    }
    let relay_mode = relay.mode();
    // The set of relay hosts this process may contact is exactly the set the
    // endpoint was composed with (D13) — so the dial map is filtered by the
    // same value the bind uses, never by a second reading of the file.
    let relay_set = crate::adapters::relay_config::relay_url_set(&relay_mode);

    // The dial map comes from the one builder (Story 18.4d, AC4), never from an
    // inline map here. ⚠ **Populating it does not make the daemon dial.** This
    // cut adds no daemon-initiated dial at all; the map is supplied so reach is
    // in place for a future dialer and so the listener and `peer ping` share one
    // source. The only exercised dialer is `peer ping`.
    let transport = std::sync::Arc::new(
        crate::adapters::iroh::IrohPeerTransport::bind(
            transport_secret_key,
            crate::adapters::p2p_reach::peer_dial_map_from_workspace(workspace, &relay_set)
                .addresses(),
            &relay_mode,
        )
        .await
        .context("binding the P2P listener")?,
    );
    let local_address = transport.local_address()?;
    let address = String::from_utf8(local_address.as_bytes().to_vec())
        .context("rendering the P2P listener address")?;

    // AC1 — publish this host's own reach, **after** a successful bind and never
    // before: a record written before the endpoint exists is a placeholder a
    // ticket would then publish as fact. ⚠ The endpoint binds an ephemeral port,
    // so this is rewritten every bind and a ticket minted during a previous run
    // may name a port nobody is listening on. A reach write failure degrades —
    // the listener still serves, and `peer invite` then emits its honest
    // no-address copy — because reach is reachability, not admission.
    if let Err(error) = crate::adapters::p2p_reach::publish_self_reach(
        &crate::infrastructure::paths::workspace_p2p_reach_path(workspace),
        &local_address,
        chrono::Utc::now().timestamp(),
    ) {
        tracing::warn!(
            %error,
            "this host's reach could not be recorded; tickets will carry no network address"
        );
    }

    // AC3 — and then keep it true. The bind-time record above is the honest
    // fact at that instant, but a relay is established *after* the bind
    // returns, so on a relay-composed host it names no relay and `peer invite`
    // would mint a ticket that names none either. ⛔ The fix is not
    // `Endpoint::online()`: with no relay configured that pends forever, which
    // is exactly the `disabled` host. It is the endpoint's own address watcher,
    // writing **only when the address actually changed** — a flapping relay
    // must not fsync this file on every WAN twitch.
    {
        let watcher_transport = transport.clone();
        let watcher_cancel = shutdown.clone();
        let reach_path = crate::infrastructure::paths::workspace_p2p_reach_path(workspace);
        tokio::spawn(async move {
            watcher_transport
                .republish_address_on_change(watcher_cancel, |address| {
                    match crate::adapters::p2p_reach::publish_self_reach_on_change(
                        &reach_path,
                        &address,
                        chrono::Utc::now().timestamp(),
                    ) {
                        Ok(true) => tracing::info!(
                            "this host's own address changed; the reach record now matches it"
                        ),
                        Ok(false) => {}
                        Err(error) => tracing::warn!(
                            %error,
                            "this host's changed reach could not be recorded; tickets keep the \
                             address already on file"
                        ),
                    }
                })
                .await;
        });
    }

    // An entry with no pinned key can never match a presented endpoint, so it
    // silently admits nobody. The refusal path names the peer that knocked, not
    // this hole, so the operator hears about it here instead.
    if let crate::domain::models::P2pConfigState::Present(peers) =
        crate::adapters::p2p_config::load_workspace_p2p_config(
            &crate::infrastructure::paths::workspace_p2p_config_path(workspace),
        )
    {
        let unpinned: Vec<&str> = peers
            .iter()
            .filter(|peer| peer.pinned_key.is_none())
            .map(|peer| peer.id.as_str())
            .collect();
        if !unpinned.is_empty() {
            tracing::warn!(
                entries = ?unpinned,
                "P2P allowlist entries have no pinned key and can admit no peer"
            );
        }
    }

    // ⚑ Story 18.4a, Rule 1 — the topic mechanism's production producer is
    // wired HERE and nowhere else: this is the first point at which a bound
    // transport exists to re-gossip on. Without this call
    // `PeerTransport::gossip_topic` has no production caller and
    // `RoomEvent::PeerEquivocated` has no producer, which is the
    // mechanism-without-a-trigger class this epic has paid for three times.
    //
    // The signer is derived from the **same** secret key the endpoint bound, so
    // the identity that signs an advertisement is the identity that carries it.
    {
        let signer = crate::adapters::rap::AgentSigner::from_signing_key(
            ed25519_dalek::SigningKey::from_bytes(&transport_secret_key),
        );
        let bound = handler.bind_topic_effects(crate::adapters::rap::TopicEffects {
            workspace: workspace.to_path_buf(),
            transport: transport.clone() as std::sync::Arc<dyn PeerTransport>,
            signer,
        });
        if !bound {
            tracing::warn!(
                "topic replication effects were already bound; this listener did not rebind them"
            );
        }
    }

    let ingress = std::sync::Arc::new(crate::adapters::iroh::IrohPeerIngress::new(
        transport.clone(),
        handler,
        workspace.to_path_buf(),
    )?);
    let task = tokio::spawn(async move {
        if let Err(error) = ingress.run(shutdown).await {
            tracing::error!(error = %error, "P2P listener stopped");
        }
        if let Err(error) = transport.shutdown().await {
            tracing::error!(error = %error, "P2P listener shutdown failed");
        }
    });
    Ok(P2pListener {
        address,
        relay,
        task,
    })
}

/// Story 18.4 AC2 — the config-gated listener is composed here, so the proof
/// that it works has to run here too. Everything below the front-door lookup is
/// exercised: bind, allowlist, shared verification, front-door delivery and
/// cancellation.
#[cfg(all(test, unix, feature = "p2p"))]
mod p2p_listener_composition_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use base64::Engine as _;
    use tokio::sync::mpsc;

    use crate::adapters::iroh::{IrohPeerTransport, derive_peer_endpoint_identity};
    use crate::adapters::rap::{
        AgentSigner, VerifiedPeerConsent, VerifiedPeerConsumer, VerifiedPeerFrameHandler,
    };
    use crate::domain::models::{AgentId, AgentMessage, CorrelationId, MessageKind, PeerId};
    use crate::domain::ports::{
        AgentMessageBus, PeerDeliveryRecord, PeerInteractionRecorder, PeerTransport,
        RelationshipDeliveryPolicy,
    };
    use crate::infrastructure::subagent::{LocalMessageBus, NodeTree};

    struct ForwardingConsumer(mpsc::UnboundedSender<String>);

    #[async_trait]
    impl VerifiedPeerConsumer for ForwardingConsumer {
        async fn consent(
            &self,
            _recipient: &AgentId,
            _content: &AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<VerifiedPeerConsent, String> {
            Ok(VerifiedPeerConsent::Accept)
        }

        async fn ingest(
            &self,
            _recipient: &AgentId,
            content: AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<(), String> {
            let _ = self.0.send(content.content);
            Ok(())
        }
    }

    struct AcceptingRecorder;

    #[async_trait]
    impl PeerInteractionRecorder for AcceptingRecorder {
        async fn record_peer_delivery(&self, _record: PeerDeliveryRecord) -> Result<(), String> {
            Ok(())
        }

        async fn record_transport_refusal(
            &self,
            _record: crate::domain::ports::TransportRefusalRecord,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn composed_listener_delivers_an_allowlisted_frame_and_stops_on_cancel() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let server_key = ed25519_dalek::SigningKey::from_bytes(&[41; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);

        let pinned = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(client_key.verifying_key().to_bytes());
        let config_dir = workspace.path().join(".rustain");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("p2p.json"),
            format!(
                r#"{{"listen":true,"agents":{{"client":{{"pinnedKey":{{"alg":"EdDSA","x":"{pinned}"}}}}}}}}"#
            ),
        )
        .expect("write allowlist");

        let (ingested_tx, mut ingested_rx) = mpsc::unbounded_channel();
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let node_tree = NodeTree::new();
        let bus = Arc::new(LocalMessageBus::new(
            node_tree.clone(),
            Arc::new(RelationshipDeliveryPolicy),
        )) as Arc<dyn AgentMessageBus>;
        let handler = Arc::new(VerifiedPeerFrameHandler::new(
            node_tree,
            Arc::new(ArcSwap::from_pointee(bus)),
            domain_tx,
            Arc::new(ForwardingConsumer(ingested_tx)),
            Arc::new(AcceptingRecorder),
        ));

        let shutdown = tokio_util::sync::CancellationToken::new();
        let listener = super::compose_p2p_listener(
            workspace.path(),
            handler,
            server_key.to_bytes(),
            shutdown.child_token(),
        )
        .await
        .expect("compose the production listener");

        let server_identity = derive_peer_endpoint_identity(&server_key.verifying_key().to_bytes())
            .expect("server identity");
        let client = IrohPeerTransport::bind(
            client_key.to_bytes(),
            HashMap::from([(
                server_identity.peer_id.clone(),
                crate::domain::ports::PeerAddress::from_bytes(listener.address.into_bytes())
                    .expect("listener address"),
            )]),
            &crate::domain::models::RelayMode::Disabled,
        )
        .await
        .expect("bind client");

        let signer = AgentSigner::from_signing_key(client_key);
        let pid = signer.identity().peer_id.as_str();
        let sender =
            AgentId::from_peer_path(&format!("{pid}/peer-transport")).expect("peer-rooted sender");
        // ⚑ Rooted at the sender's own namespace (the recipient rule 18.4a
        // enforces, `DF-18-4d-RECIPIENT-NAMESPACE`); pre-18.4a this fixture
        // addressed a bare `local-recipient`, which is refused now.
        let recipient = AgentId::from_peer_path(&format!("{pid}/local-recipient"))
            .expect("peer-rooted recipient");
        let not_after =
            crate::domain::clock::Clock::wall_now_ms(&crate::domain::clock::SystemClock::default())
                + 60_000;
        let envelope = signer
            .sign(
                sender,
                recipient.clone(),
                CorrelationId::new("composition-1"),
                MessageKind::PeerMessage,
                String::new(),
                1,
                not_after,
                "composition-nonce".to_owned(),
                Vec::new(),
                serde_json::json!("through the composed listener"),
            )
            .expect("sign envelope");
        client
            .send_to(&server_identity.peer_id, envelope)
            .await
            .expect("send to the composed listener");

        let delivered =
            tokio::time::timeout(std::time::Duration::from_secs(10), ingested_rx.recv())
                .await
                .expect("the composed listener must deliver within the test budget")
                .expect("ingest channel stays open");
        assert_eq!(delivered, "through the composed listener");

        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(10), listener.task)
            .await
            .expect("cancellation stops the composed listener")
            .expect("listener task does not panic");
        client.shutdown().await.expect("shutdown client");
    }

    /// Story 18.4d AC1 — the listener publishes this host's own reach **at
    /// bind**, through the production composition path.
    ///
    /// Mutants: (a) skipping the write leaves the store absent, so a ticket
    /// carries no address; (b) writing before `bind` succeeds would persist a
    /// placeholder — asserted by requiring the recorded bundle to be one `bind`
    /// accepts; (c) `listen: false` never reaches this path at all, which the
    /// sibling test below holds.
    #[tokio::test]
    async fn the_composed_listener_publishes_its_own_reach_at_bind() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let server_key = ed25519_dalek::SigningKey::from_bytes(&[45; 32]);
        let config_dir = workspace.path().join(".rustain");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("p2p.json"),
            r#"{"listen":true,"agents":{}}"#,
        )
        .expect("write allowlist");

        let reach_path = crate::infrastructure::paths::workspace_p2p_reach_path(workspace.path());
        assert_eq!(
            crate::adapters::p2p_reach::load_workspace_p2p_reach(&reach_path),
            crate::domain::models::PeerReachState::Absent,
            "positive control: nothing has published reach yet"
        );

        let (ingested_tx, _ingested_rx) = mpsc::unbounded_channel();
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let node_tree = NodeTree::new();
        let bus = Arc::new(LocalMessageBus::new(
            node_tree.clone(),
            Arc::new(RelationshipDeliveryPolicy),
        )) as Arc<dyn AgentMessageBus>;
        let handler = Arc::new(VerifiedPeerFrameHandler::new(
            node_tree,
            Arc::new(ArcSwap::from_pointee(bus)),
            domain_tx,
            Arc::new(ForwardingConsumer(ingested_tx)),
            Arc::new(AcceptingRecorder),
        ));
        let shutdown = tokio_util::sync::CancellationToken::new();
        let listener = super::compose_p2p_listener(
            workspace.path(),
            handler,
            server_key.to_bytes(),
            shutdown.child_token(),
        )
        .await
        .expect("compose the production listener");

        let state = crate::adapters::p2p_reach::load_workspace_p2p_reach(&reach_path);
        let own = state
            .own()
            .expect("binding the listener must publish this host's reach");
        assert!(own.captured_at > 0, "the record carries a capture stamp");
        assert_eq!(
            own.address.as_bytes(),
            listener.address.as_bytes(),
            "the published record must be the address the listener actually bound"
        );
        // Mutant (b): a placeholder written before bind would not be dialable.
        // This proves the recorded bundle is consumable, not merely non-empty.
        let identity = derive_peer_endpoint_identity(&server_key.verifying_key().to_bytes())
            .expect("server identity");
        IrohPeerTransport::bind(
            ed25519_dalek::SigningKey::from_bytes(&[46; 32]).to_bytes(),
            HashMap::from([(identity.peer_id, own.address.clone())]),
            &crate::domain::models::RelayMode::Disabled,
        )
        .await
        .expect("the published reach must be an address `bind` accepts")
        .shutdown()
        .await
        .expect("shutdown probe");

        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(10), listener.task)
            .await
            .expect("cancellation stops the composed listener")
            .expect("listener task does not panic");
    }
}

/// Real-process crash arming seam used by the Story 17.2a L2 harness. The
/// environment variable is intentionally undocumented and byte-exact; absent
/// in production, this is a zero-cost no-op.
#[cfg(unix)]
async fn arm_node_recovery_harness(
    server: &std::sync::Arc<crate::adapters::daemon::server::AttachServer>,
) -> Result<()> {
    if let Some(raw) =
        crate::infrastructure::utils::env_var_trimmed("RUSTAIN_TEST_ARM_CASCADE_RECOVERY")
    {
        let (parent_raw, child_raw) = raw
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("cascade recovery arm requires parent,child"))?;
        let parent = crate::domain::models::AgentId::parse(parent_raw)
            .map_err(|error| anyhow::anyhow!("invalid cascade parent id: {error}"))?;
        let child = crate::domain::models::AgentId::parse(child_raw)
            .map_err(|error| anyhow::anyhow!("invalid cascade child id: {error}"))?;
        let tree = server.node_tree();

        let (parent_tx, parent_rx) = tokio::sync::mpsc::channel(1);
        parent_tx
            .try_send(crate::domain::models::Op::ReportFull)
            .map_err(|error| anyhow::anyhow!("arming blocked parent channel: {error}"))?;
        tokio::spawn(async move {
            let _parent_rx = parent_rx;
            std::future::pending::<()>().await;
        });
        let (parent_status, _) =
            tokio::sync::watch::channel(crate::domain::models::NodeState::Created);
        let (_, parent_metrics) =
            tokio::sync::watch::channel(crate::domain::models::AgentMetrics::default());
        tree.register(
            parent.clone(),
            crate::domain::models::AgentId::root(),
            crate::infrastructure::subagent::AgentHandle {
                isolated: false,
                agent_id: parent.clone(),
                token: crate::domain::models::CapabilityTokenId::root(),
                command_tx: parent_tx,
                cancel_token: tokio_util::sync::CancellationToken::new(),
                depth: 1,
                subagent_type: "cascade-recovery-parent".into(),
                spawned_at: chrono::Utc::now().timestamp_millis(),
                status: parent_status,
                metrics: parent_metrics,
                mailbox_budget: crate::infrastructure::subagent::MailboxBudget::new(),
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("registering cascade parent: {error}"))?;

        let (child_tx, child_rx) = tokio::sync::mpsc::channel(1);
        drop(child_rx);
        let (child_status, _) =
            tokio::sync::watch::channel(crate::domain::models::NodeState::Created);
        let (_, child_metrics) =
            tokio::sync::watch::channel(crate::domain::models::AgentMetrics::default());
        tree.register(
            child.clone(),
            parent.clone(),
            crate::infrastructure::subagent::AgentHandle {
                isolated: false,
                agent_id: child.clone(),
                token: crate::domain::models::CapabilityTokenId::root(),
                command_tx: child_tx,
                cancel_token: tokio_util::sync::CancellationToken::new(),
                depth: 2,
                subagent_type: "cascade-recovery-child".into(),
                spawned_at: chrono::Utc::now().timestamp_millis(),
                status: child_status,
                metrics: child_metrics,
                mailbox_budget: crate::infrastructure::subagent::MailboxBudget::new(),
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("registering cascade child: {error}"))?;
        tree.set_state(&parent, crate::domain::models::NodeState::Running)
            .await;
        tree.set_state(&child, crate::domain::models::NodeState::Running)
            .await;
        tokio::spawn(async move {
            let _ = tree
                .cascade_kill(&parent, std::time::Duration::from_secs(30))
                .await;
        });
        return Ok(());
    }
    let Some(raw_id) =
        crate::infrastructure::utils::env_var_trimmed("RUSTAIN_TEST_ARM_NODE_RECOVERY")
    else {
        return Ok(());
    };
    let agent_id = crate::domain::models::AgentId::parse(&raw_id)
        .map_err(|error| anyhow::anyhow!("invalid recovery harness node id: {error}"))?;
    let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
    let (status, _status_rx) =
        tokio::sync::watch::channel(crate::domain::models::NodeState::Created);
    let (_metrics_tx, metrics) =
        tokio::sync::watch::channel(crate::domain::models::AgentMetrics::default());
    let handle = crate::infrastructure::subagent::AgentHandle {
        isolated: false,
        agent_id: agent_id.clone(),
        token: crate::domain::models::CapabilityTokenId::root(),
        command_tx,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        depth: 1,
        subagent_type: "node-recovery-harness".into(),
        spawned_at: chrono::Utc::now().timestamp_millis(),
        status,
        metrics,
        mailbox_budget: crate::infrastructure::subagent::MailboxBudget::new(),
    };
    let tree = server.node_tree();
    tree.register(
        agent_id.clone(),
        crate::domain::models::AgentId::root(),
        handle,
    )
    .await
    .map_err(|error| anyhow::anyhow!("registering recovery harness node: {error}"))?;
    tree.set_state(&agent_id, crate::domain::models::NodeState::Running)
        .await;
    Ok(())
}

/// `daemon stop` — SIGTERM, wait up to 5s for exit + PID-file removal (NFR48),
/// then escalate to SIGKILL and report the timeout (AC-12-1a-3).
#[cfg(unix)]
async fn run_daemon_stop(workspace: PathBuf) -> Result<()> {
    use std::time::{Duration, Instant};

    let pid_path = crate::infrastructure::paths::daemon_pid_path(&workspace)?;
    let pf = match pidfile::check_running(&pid_path) {
        GuardOutcome::Running(pf) => pf,
        GuardOutcome::Stale => {
            pidfile::remove(&pid_path);
            println!("Daemon not running (cleaned up stale PID file).");
            return Ok(());
        }
        GuardOutcome::Free => {
            println!("Daemon not running.");
            return Ok(());
        }
    };

    // SAFETY: kill() with a real signal; no memory touched.
    unsafe {
        let rc = libc::kill(pf.pid as libc::pid_t, libc::SIGTERM);
        if rc == -1 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) {
                anyhow::bail!(
                    "permission denied sending SIGTERM to PID {} — \
                     the daemon may be owned by a different user",
                    pf.pid
                );
            }
        }
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !pidfile::process_alive(pf.pid) {
            pidfile::remove(&pid_path);
            socket::cleanup(&pf.socket_path);
            println!("Daemon stopped (PID: {}).", pf.pid);
            return Ok(());
        }
        if Instant::now() >= deadline {
            // SAFETY: see above.
            unsafe {
                libc::kill(pf.pid as libc::pid_t, libc::SIGKILL);
            }
            pidfile::remove(&pid_path);
            socket::cleanup(&pf.socket_path);
            eprintln!(
                "Daemon did not exit within 5s; escalated to SIGKILL (PID: {}).",
                pf.pid
            );
            anyhow::bail!("daemon stop timed out; sent SIGKILL");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `daemon status` — structured snapshot (AC-12-1a-2). Not-running prints a clear
/// line and exits non-zero so scripts can branch.
#[cfg(unix)]
async fn run_daemon_status(workspace: PathBuf, config: AppConfig, json: bool) -> Result<()> {
    let pid_path = crate::infrastructure::paths::daemon_pid_path(&workspace)?;
    let pf = match pidfile::check_running(&pid_path) {
        GuardOutcome::Running(pf) => pf,
        GuardOutcome::Stale | GuardOutcome::Free => {
            if json {
                println!("{}", serde_json::json!({ "running": false }));
            } else {
                println!("Daemon not running.");
            }
            anyhow::bail!("daemon not running");
        }
    };

    let snapshot = status::StatusSnapshot::gather(&pf, &config);
    if json {
        println!("{}", snapshot.to_json());
    } else {
        println!("{}", snapshot.to_human());
    }
    Ok(())
}

/// The invoking user, for `--system` systemd `User=`. `$USER`/`$LOGNAME` with a
/// `root` fallback (the only plausible `--system` install context anyway).
#[cfg(unix)]
fn invoking_user() -> String {
    use crate::infrastructure::utils::env_var_trimmed;
    env_var_trimmed("USER")
        .or_else(|| env_var_trimmed("LOGNAME"))
        .unwrap_or_else(|| "root".to_string())
}

/// `daemon install` (AC-12-1b-3) — render the platform service file and either print
/// it (`--print`, stdout only) or write it to the resolved location + print the
/// follow-up commands. Pure generate; no memory composition, no daemon runtime state.
#[cfg(unix)]
fn run_daemon_install(
    workspace: PathBuf,
    config: AppConfig,
    print: bool,
    system: bool,
) -> Result<()> {
    use crate::infrastructure::paths;

    let exe = std::env::current_exe()
        .and_then(|p| {
            p.canonicalize().map_err(|e| {
                std::io::Error::new(e.kind(), format!("canonicalizing {}: {e}", p.display()))
            })
        })
        .context("resolving the rustain executable path (current_exe)")?
        .display()
        .to_string();
    let params = service::ServiceParams {
        exe,
        profile: config.active_profile.clone(),
        workspace: workspace.display().to_string(),
        user: invoking_user(),
        system,
        log_path: paths::daemon_log_path(&workspace)?.display().to_string(),
        label: paths::daemon_service_label(&workspace),
        // Pass env overrides through ONLY when set in the generating environment
        // (AC-12-1b-1): test/CI overrides survive; default installs rely on $HOME.
        data_dir: crate::infrastructure::utils::env_var_trimmed("RUSTAIN_DATA_DIR"),
        config_dir: crate::infrastructure::utils::env_var_trimmed("RUSTAIN_CONFIG_DIR"),
    };

    #[cfg(target_os = "macos")]
    let rendered = service::render_launchd_plist(&params);
    #[cfg(not(target_os = "macos"))]
    let rendered = service::render_systemd_unit(&params);

    if print {
        // stdout ONLY — no filesystem write (for inspection / piping).
        print!("{rendered}");
        return Ok(());
    }

    let dest = paths::daemon_service_path(&workspace, system)?;
    if let Some(parent) = dest.parent() {
        // User scope: create `~/.config/systemd/user` (or LaunchAgents). System scope:
        // /etc/systemd/system already exists; create_dir_all is a no-op there.
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating service dir {}", parent.display()))?;
    }
    std::fs::write(&dest, &rendered)
        .with_context(|| format!("writing service file {}", dest.display()))?;

    println!("Installed service file: {}", dest.display());
    let name = paths::daemon_service_file_name(&workspace);
    println!("\nNext, enable + start it:");
    #[cfg(target_os = "macos")]
    {
        let _ = (system, &name);
        println!("  launchctl load {}", dest.display());
    }
    #[cfg(not(target_os = "macos"))]
    {
        if system {
            println!("  sudo systemctl daemon-reload && sudo systemctl enable --now {name}");
        } else {
            println!("  systemctl --user daemon-reload && systemctl --user enable --now {name}");
        }
    }
    Ok(())
}

/// `daemon uninstall` (AC-12-1b-3b) — remove the workspace's service file
/// idempotently (missing file → exit 0 no-op) and print the disable/unload follow-up.
/// Touches NO daemon runtime state (PID file/socket/crash records are the lifecycle's
/// concern, not the installer's).
#[cfg(unix)]
fn run_daemon_uninstall(workspace: PathBuf, system: bool) -> Result<()> {
    use crate::infrastructure::paths;

    let dest = paths::daemon_service_path(&workspace, system)?;
    if !dest.exists() {
        // Idempotent: a second uninstall (or never-installed) is a success no-op.
        println!(
            "No service file installed for this workspace ({}).",
            dest.display()
        );
        return Ok(());
    }

    let name = paths::daemon_service_file_name(&workspace);
    println!("First disable + stop the running service (recommended):");
    #[cfg(target_os = "macos")]
    {
        let _ = (system, &name);
        println!("  launchctl unload {}", dest.display());
    }
    #[cfg(not(target_os = "macos"))]
    {
        if system {
            println!("  sudo systemctl disable --now {name}");
        } else {
            println!("  systemctl --user disable --now {name}");
        }
    }

    if let Err(e) = std::fs::remove_file(&dest) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e).with_context(|| format!("removing service file {}", dest.display()));
        }
        // Concurrent uninstall already removed it — idempotent success.
        println!(
            "No service file installed for this workspace ({}).",
            dest.display()
        );
    } else {
        println!("Removed service file: {}", dest.display());
    }
    Ok(())
}

#[cfg(unix)]
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Story 12.2b AC4 — restore the daemon's per-process conversation from the
/// workspace session (the most-recently-updated one), or start a fresh one. A
/// fresh conversation is persisted immediately so `status`/re-attach see it.
#[cfg(unix)]
async fn load_or_new_conversation(
    storage: &dyn crate::domain::ports::StoragePort,
) -> crate::domain::models::Conversation {
    use crate::domain::models::Conversation;
    if let Ok(mut summaries) = storage.list_conversations().await {
        summaries.sort_by_key(|s| s.updated_at);
        if let Some(latest) = summaries.last() {
            match storage.load_conversation(&latest.id).await {
                Ok(Some(conv)) => {
                    tracing::info!(id = %conv.id, "daemon: restored per-process conversation");
                    return conv;
                }
                Ok(None) => {
                    tracing::warn!(id = %latest.id, "daemon: latest conversation not found — starting fresh");
                }
                Err(e) => {
                    tracing::warn!(error = %e, id = %latest.id, "daemon: loading latest conversation failed — starting fresh");
                }
            }
        }
    }
    let now = now_unix() as i64;
    let conv = Conversation {
        id: crate::domain::models::generate_conversation_id(),
        title: "daemon".to_string(),
        created_at: now,
        updated_at: now,
        ..Default::default()
    };
    if let Err(e) = storage.save_conversation(&conv).await {
        tracing::warn!(error = %e, "daemon: persisting fresh conversation failed");
    }
    conv
}
