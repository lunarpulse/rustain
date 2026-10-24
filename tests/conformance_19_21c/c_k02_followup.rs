//! Story 19.21c K02: followups are sends, never model prompts or S7 refusals.
use super::attached_harness::{AttachedClient, AttachedDaemon};
use rustain::adapters::daemon::protocol::{ClientFrame, ConnectionTier, DaemonFrame};

#[tokio::test]
async fn k02_followup_is_a_daemon_send_not_s7() {
    let daemon = AttachedDaemon::start(vec![]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let frame: ClientFrame = serde_json::from_value(serde_json::json!({
        "teamFollowup": {"request":"followup-1", "peer":"missing", "reply_to":"turn-1", "text":"more"}
    })).expect("followup must have a daemon frame, not the S7 refusal");
    writer.send_frame(frame).await;
    let answer = writer.wait_send_settled("followup-1", 0).await;
    assert!(matches!(answer, DaemonFrame::RecipientActionResult { .. }));
    assert!(daemon.requests.lock().is_empty());
    writer.detach().await;
    daemon.stop().await;
}

#[tokio::test]
async fn k02_followup_links_one_recipient_without_action_or_started_and_gates_writes() {
    use super::{attached_harness::RecipientHost, c_k01_thread::seed, harness::read_room_events};
    use rustain::adapters::{
        a2a::admission::A2aAdmissionPolicy,
        daemon::protocol::{RecipientAction, RecipientActionTarget, TeamSendOutcome},
    };
    use rustain::domain::models::RoomEvent;
    let host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![host.peer("B")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let mut reader = AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    let mut peer = AttachedClient::attach(&daemon.path, ConnectionTier::Peer, true).await;
    let item = seed(&mut writer, &host, "ordinary-control").await;
    let actions = |rows: &[(u64, RoomEvent)]| {
        rows.iter()
            .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
            .count()
    };
    assert_eq!(actions(&read_room_events(&daemon.journal).await), 1);
    assert!(
        writer
            .frames_for_request("ordinary-control")
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamSendStarted { .. }))
    );
    writer
        .send_frame(ClientFrame::TeamFollowup {
            request: "linked".into(),
            peer: "B".into(),
            reply_to: item.clone(),
            text: "human followup".into(),
        })
        .await;
    assert!(matches!(
        writer.wait_send_settled("linked", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    assert!(
        !writer
            .frames_for_request("linked")
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamSendStarted { .. }))
    );
    assert_eq!(actions(&read_room_events(&daemon.journal).await), 1);
    assert!(read_room_events(&host.journal).await.iter().any(|(_, event)|
        matches!(event, RoomEvent::RecipientItemReceived { content, in_reply_to: Some(root), .. }
            if content == "human followup" && root == &item)));
    let sends = host.method_count("message/send");
    for (client, key, alias) in [
        (&mut reader, "reader-followup", "B"),
        (&mut peer, "peer-followup", "B"),
        (&mut writer, "invalid-followup", "missing"),
    ] {
        client
            .send_frame(ClientFrame::TeamFollowup {
                request: key.into(),
                peer: alias.into(),
                reply_to: item.clone(),
                text: "more".into(),
            })
            .await;
        assert!(matches!(client.wait_send_settled(key, 0).await,
            DaemonFrame::RecipientActionResult { action: RecipientAction::TeamSend,
                target: RecipientActionTarget::Request { request }, .. } if request == key));
    }
    assert_eq!(
        host.method_count("message/send"),
        sends,
        "refusals perform no I/O"
    );
    assert!(daemon.requests.lock().is_empty());
    peer.detach().await;
    reader.detach().await;
    writer.detach().await;
    daemon.stop().await;
    host.stop().await;
}
