//! Production noninteractive entrypoints, with a real trusted socket writer.
//! Neither test constructs a TurnRoute or calls a scheduler/tool bypass.

use super::attached_harness::FIXTURE_TIMEOUT;
use super::g_harness::{completed, fence, fixture, input, proposal, send_once, submit, zero};
use rustain::adapters::daemon::protocol::DaemonFrame;
use rustain::domain::models::{ChannelKind, ChannelTurnRequest};

const OPERATOR_ONLY: &str = "propose_team_message is only available to the operator's own agent";

fn results_since(daemon: &super::attached_harness::AttachedDaemon, before: usize) -> String {
    daemon.requests.lock()[before..]
        .iter()
        .flat_map(|(messages, _)| messages.iter())
        .flat_map(|message| message.tool_results.iter())
        .map(|result| result.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn channel_production_entry_denies_proposal_with_trusted_writer() {
    let (daemon, mut writer, host) = fixture().await;
    let before = daemon.provider_request_count();
    daemon.script.tool("propose_team_message", input());
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    // This is the same adapter-to-daemon channel consumed by AttachServer::run
    // in production (formerly run_with_channels), not drive_turn with a forged origin.
    daemon
        .channel_tx
        .send(ChannelTurnRequest {
            text: "Please propose the exact supplied team message.".into(),
            origin: ChannelKind::Telegram,
            response_tx,
        })
        .expect("live production channel receiver");
    tokio::time::timeout(FIXTURE_TIMEOUT, response_rx)
        .await
        .expect("channel completion deadline")
        .expect("production channel response callback must resolve");
    // The callback can resolve at an intermediate assistant fold. Observe the
    // real terminal provider event before inspecting its tool-result history.
    completed(&mut writer).await;
    let result = results_since(&daemon, before);
    assert!(
        result.contains(OPERATOR_ONLY),
        "M40 channel production entry must deny the proposal as operator-only: {result}"
    );
    fence(&mut writer, "channel-denial-fence").await;
    assert!(
        daemon.proposals.pending().await.is_empty(),
        "M40 channel production entry must retain zero proposals"
    );
    assert!(
        !writer
            .received
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamProposal { .. })),
        "M40 channel production entry must publish zero proposal frames"
    );
    zero(
        &daemon,
        &[&host],
        "M40 channel production entry must make zero recipient GET/POST or send journal effects",
    )
    .await;

    // The very same provider, scheduler, registry, egress and attached writer
    // must raise and send when the production front door is interactive.
    let result = submit(&daemon, &mut writer, input()).await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "channel denial positive control must raise through attached UserMessage: {result}"
    );
    let id = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}

#[cfg(feature = "cron")]
#[tokio::test]
async fn cron_scheduler_job_denies_proposal_with_trusted_writer() {
    use rustain::adapters::noop::NoOpChannel;
    use rustain::adapters::scheduler::cron::CronSchedulerAdapter;
    use rustain::domain::ports::SchedulerPort;
    use std::sync::Arc;

    let (daemon, mut writer, host) = fixture().await;
    let before = daemon.provider_request_count();
    daemon.script.tool("propose_team_message", input());
    let cron_path = daemon.journal.workspace.path().join("cron.toml");
    tokio::fs::write(&cron_path,
        "[[jobs]]\nname = \"proposal-denial\"\nschedule = \"* * * * * *\"\nprompt = \"Please propose the exact supplied team message.\"\nforward = false\n")
        .await.expect("write real cron job configuration");
    let (completion_tx, mut completion_rx) = tokio::sync::mpsc::unbounded_channel();
    // load/start_loop -> run_scheduler_loop -> maybe_spawn_job -> run_job ->
    // DaemonTurnRuntime is the genuine cron production execution chain.
    let scheduler = CronSchedulerAdapter::load(
        cron_path,
        daemon.core.clone(),
        completion_tx,
        Arc::new(NoOpChannel),
        daemon.runtime.storage.clone(),
    )
    .await
    .expect("load actual cron scheduler");
    scheduler
        .start_loop()
        .await
        .expect("start production cron scheduling");
    let completion = tokio::time::timeout(FIXTURE_TIMEOUT, completion_rx.recv())
        .await
        .expect("cron completion deadline")
        .expect("production cron job must complete");
    scheduler
        .shutdown_loop()
        .await
        .expect("shutdown and join actual cron jobs");
    assert_eq!(
        completion.job_name, "proposal-denial",
        "cron denial must come from the configured executed job"
    );
    let result = results_since(&daemon, before);
    assert!(
        result.contains(OPERATOR_ONLY),
        "M40 cron scheduler job must deny the proposal as operator-only: {result}"
    );
    fence(&mut writer, "cron-denial-fence").await;
    assert!(
        daemon.proposals.pending().await.is_empty(),
        "M40 cron scheduler job must retain zero proposals"
    );
    assert!(
        !writer
            .received
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamProposal { .. })),
        "M40 cron scheduler job must publish zero proposal frames"
    );
    zero(
        &daemon,
        &[&host],
        "M40 cron scheduler job must make zero recipient GET/POST or send journal effects",
    )
    .await;

    let result = submit(&daemon, &mut writer, input()).await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "cron denial positive control must raise through attached UserMessage: {result}"
    );
    let id = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
