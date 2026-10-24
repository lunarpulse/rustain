//! Story 19.21c K01: signed socket reads, read-only authority and origin privacy.
use super::attached_harness::{AttachedClient, AttachedDaemon, RecipientHost};
use super::harness::read_room_events;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionTarget,
    TeamSendOutcome,
};
use rustain::domain::models::RoomEvent;

pub(super) async fn seed(writer: &mut AttachedClient, host: &RecipientHost, key: &str) -> String {
    writer
        .send_team(key, vec!["B".into()], "hello", None, None)
        .await;
    assert!(matches!(
        writer.wait_send_settled(key, 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    read_room_events(&host.journal)
        .await
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address, content, ..
            } if content == "hello" => Some(address.item().as_str().to_owned()),
            _ => None,
        })
        .expect("real recipient ingress")
}

pub(super) async fn fence(daemon: &AttachedDaemon, client: &mut AttachedClient) {
    daemon
        .room_journal
        .record_event(RoomEvent::Unrecognized)
        .await
        .unwrap();
    client
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::Event(raw) if matches!(raw.kind,
        rustain::infrastructure::runtime::event_bus::RawEventKind::DomainEvent(
            rustain::domain::events::DomainEventPayload::Room(RoomEvent::Unrecognized))))
        })
        .await;
}

const PRIVATE_REPLY: &str = "distinctive private peer reply";

async fn observed_reply_rows(daemon: &AttachedDaemon) -> usize {
    read_room_events(&daemon.journal)
        .await
        .into_iter()
        .filter(|(_, event)| {
            matches!(event, RoomEvent::PeerReplyObserved { content: Some(body), .. } if body == PRIVATE_REPLY)
        })
        .count()
}

#[tokio::test]
async fn k01_thread_is_a_daemon_view_not_s7() {
    let daemon = AttachedDaemon::start(vec![]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let frame: ClientFrame = serde_json::from_value(serde_json::json!({
        "teamThread": {"request":"thread-1", "peer":"missing", "item_id":"ri_missing"}
    }))
    .expect("thread must have a daemon frame, not the S7 refusal");
    writer.send_frame(frame).await;
    let view = writer
        .wait_board_result("thread-1")
        .await
        .expect("thread view");
    assert!(!view.is_empty());
    assert!(daemon.requests.lock().is_empty());
    writer.detach().await;
    daemon.stop().await;
}

#[tokio::test]
async fn k01_writer_reader_standalone_bytes_peer_gate_and_reply_privacy() {
    let mut host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let recipient_socket = host.enable_operator().await;
    let mut recipient_writer =
        AttachedClient::attach(&recipient_socket, ConnectionTier::TrustedLocal, false).await;
    let daemon = AttachedDaemon::start(vec![host.peer("B")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let mut reader = AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    let mut peer = AttachedClient::attach(&daemon.path, ConnectionTier::Peer, true).await;
    let mut observer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    let item = seed(&mut writer, &host, "seed-thread").await;
    recipient_writer
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: item.clone(),
            text: PRIVATE_REPLY.into(),
        })
        .await;
    recipient_writer
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::RecipientActionResult {
        action: RecipientAction::Reply, target: RecipientActionTarget::Item { item_id }, ..
    } if item_id == &item)
        })
        .await;
    for (client, request) in [
        (&mut writer, "thread-writer"),
        (&mut reader, "thread-reader"),
    ] {
        client
            .send_frame(ClientFrame::TeamThread {
                request: request.into(),
                peer: "B".into(),
                item_id: item.clone(),
            })
            .await;
        let actual = client.wait_board_result(request).await.unwrap();
        // The attached read itself journals the reply; the oracle below shares
        // this journal, so the row must exist before the oracle ever runs.
        assert_eq!(
            observed_reply_rows(&daemon).await,
            1,
            "attached {request} read journals the reply exactly once"
        );
        let expected = rustain::infrastructure::runtime::transparency_bridge::team_thread_message(
            daemon.egress.runtime(),
            "B",
            &item,
        )
        .await;
        assert_eq!(actual, expected);
        assert!(actual.contains("thread · B ·"));
        assert!(actual.contains("you"));
        assert!(actual.contains(PRIVATE_REPLY));
    }
    let reads = host.method_count("x-rustain-items/thread");
    peer.send_frame(ClientFrame::TeamThread {
        request: "peer-thread".into(),
        peer: "B".into(),
        item_id: item.clone(),
    })
    .await;
    let refusal = peer
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::RecipientActionResult {
        action: RecipientAction::TeamThread, target: RecipientActionTarget::Request { request }, ..
    } if request == "peer-thread")
        })
        .await;
    assert!(
        serde_json::to_string(&refusal)
            .unwrap()
            .contains("trusted-local only")
    );
    assert_eq!(host.method_count("x-rustain-items/thread"), reads);
    assert_eq!(
        observed_reply_rows(&daemon).await,
        1,
        "repeat writer/reader/oracle reads journal a reply once"
    );
    // The origin reader may explicitly read the view; a fresh observer and Peer may not.
    writer
        .send_frame(ClientFrame::TeamThread {
            request: "thread-private".into(),
            peer: "B".into(),
            item_id: item,
        })
        .await;
    assert!(
        writer
            .wait_board_result("thread-private")
            .await
            .unwrap()
            .contains(PRIVATE_REPLY)
    );
    fence(&daemon, &mut observer).await;
    fence(&daemon, &mut peer).await;
    for client in [&observer, &peer] {
        assert!(
            !serde_json::to_string(&client.received)
                .unwrap()
                .contains(PRIVATE_REPLY)
        );
        assert!(client.frames_for_request("thread-writer").is_empty());
        assert!(client.frames_for_request("thread-private").is_empty());
    }
    assert!(daemon.requests.lock().is_empty());
    assert_eq!(
        daemon
            .model_turns
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    observer.detach().await;
    peer.detach().await;
    reader.detach().await;
    writer.detach().await;
    daemon.stop().await;
    recipient_writer.detach().await;
    host.stop().await;
}
