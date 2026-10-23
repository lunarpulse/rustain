use super::attached_harness::{
    AttachedClient, AttachedDaemon, InboundSenderIdentity, pinned_peer_for_inbound_sender,
};
use super::g_harness::*;
use super::harness::Journal;
use super::observed_host::ObservedHost;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{ClientFrame, ConnectionTier, DaemonFrame};
use rustain::domain::models::{ResponseMode, RoomEvent};

pub(crate) async fn fixture_with_remote() -> (
    AttachedDaemon,
    AttachedClient,
    ObservedHost,
    super::attached_harness::InboundHttpHost,
) {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let journal = Journal::open().await;
    let identity = InboundSenderIdentity::new("remote-drafter");
    let directory = journal.workspace.path().join(".rustain");
    std::fs::create_dir_all(&directory).expect("workspace policy directory");
    std::fs::write(directory.join(rustain::domain::models::INDIVIDUAL_POLICY_FILE),format!(
        "[interaction.overrides.\"remote-drafter\"]\npeer_id = \"{}\"\nresponse_mode = \"notify-and-draft\"\n",identity.identity.peer_id.as_str())).expect("real NotifyAndDraft policy");
    let daemon = AttachedDaemon::start_with_journal(
        journal,
        vec![
            host.peer("lena-po"),
            pinned_peer_for_inbound_sender(&identity),
        ],
        None,
    )
    .await;
    let inbound = daemon.serve_http(vec![identity]).await;
    let writer = AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    host.reset_counts();
    (daemon, writer, host, inbound)
}

pub(crate) async fn peer_ready(daemon: &AttachedDaemon) {
    tokio::time::timeout(super::attached_harness::FIXTURE_TIMEOUT, async {
        loop {
            if daemon
                .conversation
                .lock()
                .await
                .peer_row_controls
                .values()
                .any(|control| control.stage == rustain::domain::models::PeerRowStage::Ready)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("production NotifyAndDraft turn must commit its Ready draft");
}
#[tokio::test]
async fn remote_notify_and_draft_tool_attempt_is_denied() {
    let (daemon, mut writer, host, inbound) = fixture_with_remote().await;
    let before = daemon.provider_request_count();
    let mut transitions = daemon.runtime.tool_scheduler.subscribe();
    daemon.script.tool("propose_team_message", input());
    let result = inbound
        .sender("remote-drafter")
        .message_send("remote-tool-attempt", "try the supplied tool")
        .await;
    assert!(
        result.json["error"].is_null(),
        "real remote turn must be admitted: {result:?}"
    );
    daemon.wait_for_received_task("remote-tool-attempt").await;
    tokio::time::timeout(super::attached_harness::FIXTURE_TIMEOUT, async {
        loop {
            let event = transitions
                .recv()
                .await
                .expect("real remote tool transition");
            if event.call.is_terminal() {
                break;
            }
        }
    })
    .await
    .expect("remote tool completes without operator intervention");
    peer_ready(&daemon).await;
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "observe remote completion before continuing".into(),
            images: vec![],
        })
        .await;
    completed(&mut writer).await;
    assert!(
        daemon.requests.lock()[before..]
            .iter()
            .flat_map(|(messages, _)| messages.iter())
            .flat_map(|message| message.tool_results.iter())
            .any(|result| result.content.contains("only available to the operator")),
        "M03: NotifyAndDraft remote proposal must be denied as operator-only"
    );
    assert!(
        daemon.proposals.pending().await.is_empty(),
        "M03: NotifyAndDraft remote proposal must be denied as operator-only"
    );
    fence(&mut writer, "remote-completed-fence").await;
    assert!(
        !writer
            .received
            .iter()
            .any(|f| matches!(f, DaemonFrame::TeamProposal { .. })),
        "M03: NotifyAndDraft remote proposal must be denied as operator-only"
    );
    zero(
        &daemon,
        &[&host],
        "M03: NotifyAndDraft remote proposal must be denied as operator-only",
    )
    .await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    inbound.stop().await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn pending_proposal_does_not_hold_daemon_turn_mutex() {
    let (daemon, mut writer, host, inbound) = fixture_with_remote().await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    let before = daemon.provider_request_count();
    let result = inbound
        .sender("remote-drafter")
        .message_send("pending-peer-turn", "peer turn while a proposal waits")
        .await;
    assert!(
        result.json["error"].is_null(),
        "M32: pending proposal must not block inbound daemon turn"
    );
    daemon.wait_for_received_task("pending-peer-turn").await;
    peer_ready(&daemon).await;
    // The next prompt's completed frame is a causal turn-lock fence, not a sleep.
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "next operator prompt after peer completion".into(),
            images: vec![],
        })
        .await;
    completed(&mut writer).await;
    assert!(
        daemon.provider_request_count() >= before + 2,
        "M32: pending proposal must not block peer and next operator turns"
    );
    assert!(
        daemon.requests.lock()[before..]
            .iter()
            .any(|(messages, _)| messages
                .iter()
                .any(|message| message.content.contains("peer turn while"))),
        "M32: peer turn must actually run its provider"
    );
    assert_eq!(
        daemon.proposals.pending().await[0].id,
        id,
        "pending proposal remains available after both turns"
    );
    zero(
        &daemon,
        &[&host],
        "M32: pending proposal cannot send during other turns",
    )
    .await;
    send_once(&daemon, &mut writer, &host, &id).await;
    assert!(
        super::harness::read_room_events(&daemon.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { .. })),
        "real peer item must be journaled"
    );
    inbound.stop().await;
    daemon.stop().await;
    host.stop().await;
}
