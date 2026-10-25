//! Story 19.21c K04/K05: compare-and-take, ordering and daemon-owned lifetime.
use super::{
    attached_harness::{AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT, RecipientHost},
    c_k01_thread::{fence, seed},
    c_k03_retract::{answered, decide, dispatches, preview},
    harness::read_room_events,
};
use rustain::adapters::{
    a2a::admission::A2aAdmissionPolicy,
    daemon::{
        protocol::{
            ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionTarget,
            TeamRetractPreviewView, TeamSendOutcome,
        },
        server::TeamSendTestHooks,
    },
};
use rustain::domain::models::RoomEvent;
const STALE: &str = "That retract is no longer awaiting your answer. Nothing was marked.";

async fn refused(client: &mut AttachedClient, key: &str, sentence: &str) {
    let frame = client
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::RecipientActionResult {
        action: RecipientAction::TeamRetract, target: RecipientActionTarget::Request { request }, ..
    } if request == key)
        })
        .await;
    let DaemonFrame::RecipientActionResult {
        outcome: rustain::adapters::daemon::protocol::RecipientActionOutcome::Refused { error },
        ..
    } = &frame
    else {
        panic!("expected refusal: {frame:?}");
    };
    assert!(error.to_string().contains(sentence), "{frame:?}");
}
async fn completion(daemon: &AttachedDaemon, key: &str) {
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            if daemon
                .team_send_hooks
                .next_completion()
                .await
                .unwrap()
                .request
                == key
            {
                break;
            }
        }
    })
    .await
    .expect("joined task completion fence");
}
async fn request_preview(client: &mut AttachedClient, key: &str, item: &str) {
    client
        .send_frame(ClientFrame::TeamRetractPreview {
            request: key.into(),
            peer: "B".into(),
            item_id: item.into(),
        })
        .await;
}

#[tokio::test]
async fn k04_stale_preserves_newer_double_confirm_decline_and_disarmed_are_gated() {
    let host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![host.peer("B")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let item = seed(&mut writer, &host, "seed-gating").await;
    preview(&mut writer, "old", &item).await;
    assert!(matches!(
        preview(&mut writer, "new", &item).await,
        TeamRetractPreviewView::Card { armed: true, .. }
    ));
    decide(&mut writer, "old", true).await;
    refused(&mut writer, "old", STALE).await;
    assert_eq!(dispatches(&daemon).await, 0);
    assert_eq!(host.method_count("x-rustain-items/retract"), 0);
    decide(&mut writer, "new", true).await;
    decide(&mut writer, "new", true).await;
    refused(&mut writer, "new", STALE).await;
    assert!(
        answered(&mut writer, "new")
            .await
            .starts_with("Retracted on B's host.")
    );
    assert_eq!(dispatches(&daemon).await, 1);
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    preview(&mut writer, "declined", &item).await;
    decide(&mut writer, "declined", false).await;
    decide(&mut writer, "declined", true).await;
    refused(&mut writer, "declined", STALE).await;
    decide(&mut writer, "unknown", true).await;
    refused(&mut writer, "unknown", STALE).await;
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    let removed = seed(&mut writer, &host, "seed-removed").await;
    let address = read_room_events(&host.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { address, .. }
                if address.item().as_str() == removed =>
            {
                Some(address)
            }
            _ => None,
        })
        .unwrap();
    // Fixture recipient housekeeping; the attached decision still enters the real socket.
    host.journal
        .room
        .record_event(RoomEvent::RecipientItemRemoved { address })
        .await
        .unwrap();
    assert!(matches!(
        preview(&mut writer, "disarmed", &removed).await,
        TeamRetractPreviewView::Card { armed: false, .. }
    ));
    decide(&mut writer, "disarmed", true).await;
    refused(&mut writer, "disarmed", "not armed").await;
    assert_eq!(dispatches(&daemon).await, 1);
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    writer.detach().await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn k04_reading_reverse_completion_and_closed_preview_never_resurrect_slots() {
    let host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![host.peer("B")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let item = seed(&mut writer, &host, "seed-order").await;
    let reading = daemon.team_send_hooks.hold_retract_preview("reading").await;
    request_preview(&mut writer, "reading", &item).await;
    reading.wait_entered().await;
    decide(&mut writer, "reading", true).await;
    refused(&mut writer, "reading", STALE).await;
    reading.release();
    completion(&daemon, "reading").await;
    let old = daemon
        .team_send_hooks
        .hold_retract_preview("slow-old")
        .await;
    let new = daemon
        .team_send_hooks
        .hold_retract_preview("slow-new")
        .await;
    request_preview(&mut writer, "slow-old", &item).await;
    old.wait_entered().await;
    request_preview(&mut writer, "slow-new", &item).await;
    new.wait_entered().await;
    new.release();
    writer.wait_for(|frame| matches!(frame, DaemonFrame::TeamRetractPreviewed { request, .. } if request == "slow-new")).await;
    old.release();
    completion(&daemon, "slow-old").await;
    decide(&mut writer, "slow-old", true).await;
    refused(&mut writer, "slow-old", STALE).await;
    decide(&mut writer, "slow-new", true).await;
    answered(&mut writer, "slow-new").await;
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    // A fresh armed item, so a faulty late completion would have a card to store.
    let fresh = seed(&mut writer, &host, "seed-closed").await;
    let closed = daemon.team_send_hooks.hold_retract_preview("closed").await;
    request_preview(&mut writer, "closed", &fresh).await;
    closed.wait_entered().await;
    writer.detach().await;
    let mut replacement =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert!(matches!(
        preview(&mut replacement, "replacement", &fresh).await,
        TeamRetractPreviewView::Card { armed: true, .. }
    ));
    closed.release();
    completion(&daemon, "closed").await;
    // The closed preview's late completion stored nothing under any connection.
    assert_eq!(daemon.server.team_retract_slot_count().await, 1);
    decide(&mut replacement, "closed", true).await;
    refused(&mut replacement, "closed", STALE).await;
    // ...and the replacement's newer slot survived: its armed confirm lands.
    decide(&mut replacement, "replacement", true).await;
    assert!(
        answered(&mut replacement, "replacement")
            .await
            .starts_with("Retracted on B's host.")
    );
    assert_eq!(daemon.server.team_retract_slot_count().await, 0);
    fence(&daemon, &mut replacement).await;
    assert!(
        replacement
            .frames_for_request("closed")
            .iter()
            .all(|frame| !matches!(frame, DaemonFrame::TeamRetractPreviewed { .. }))
    );
    assert_eq!(dispatches(&daemon).await, 2);
    assert_eq!(host.method_count("x-rustain-items/retract"), 2);
    replacement.detach().await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn k05_thread_served_at_eight_sends_retract_consumed_on_saturation() {
    use rustain::adapters::daemon::server::TeamSendTestHooks;
    let host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let hooks = TeamSendTestHooks::new(None, None, false).with_send_action_gate();
    let daemon = AttachedDaemon::start_with_test_hooks(vec![host.peer("B")], hooks.clone()).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    writer
        .send_team(
            "seed-saturation",
            vec!["B".into()],
            "hello",
            None,
            None,
            None,
            None,
        )
        .await;
    hooks.wait_for_send_action_count(1).await;
    hooks.release_send_actions(1);
    assert!(matches!(
        writer.wait_send_settled("seed-saturation", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    completion(&daemon, "seed-saturation").await;
    let item = read_room_events(&host.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { address, .. } => {
                Some(address.item().as_str().to_owned())
            }
            _ => None,
        })
        .unwrap();
    for index in 0..8 {
        writer
            .send_team(
                format!("held-{index}"),
                vec!["B".into()],
                "held",
                None,
                None,
                None,
                None,
            )
            .await;
    }
    hooks.wait_for_send_action_count(9).await;
    writer
        .send_frame(ClientFrame::TeamFollowup {
            request: "followup-busy".into(),
            peer: "B".into(),
            reply_to: item.clone(),
            text: "more".into(),
        })
        .await;
    assert!(matches!(
        writer.wait_send_settled("followup-busy", 0).await,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::TeamSend,
            ..
        }
    ));
    assert_eq!(host.method_count("message/send"), 1);
    writer
        .send_frame(ClientFrame::TeamThread {
            request: "thread-saturated".into(),
            peer: "B".into(),
            item_id: item.clone(),
        })
        .await;
    assert!(
        writer
            .wait_board_result("thread-saturated")
            .await
            .unwrap()
            .contains("thread · B ·")
    );
    preview(&mut writer, "busy", &item).await;
    decide(&mut writer, "busy", true).await;
    refused(&mut writer, "busy", "limit reached").await;
    decide(&mut writer, "busy", true).await;
    refused(&mut writer, "busy", STALE).await;
    assert_eq!(host.method_count("x-rustain-items/retract"), 0);
    assert_eq!(dispatches(&daemon).await, 0);
    hooks.release_send_actions(8);
    for index in 0..8 {
        assert!(matches!(
            writer.wait_send_settled(&format!("held-{index}"), 0).await,
            DaemonFrame::TeamSendSettled {
                outcome: TeamSendOutcome::Delivered,
                ..
            }
        ));
    }
    // Completion returns permits before the next control preview/decision.
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        let mut completed = std::collections::HashSet::new();
        while completed.len() < 8 {
            let event = daemon.team_send_hooks.next_completion().await.unwrap();
            if event.request.starts_with("held-") {
                completed.insert(event.request);
            }
        }
    })
    .await
    .expect("all held sends return permits");
    preview(&mut writer, "control", &item).await;
    decide(&mut writer, "control", true).await;
    answered(&mut writer, "control").await;
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    assert!(daemon.requests.lock().is_empty());
    writer.detach().await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn k05_confirmed_dispatch_survives_detach_and_drops_result() {
    let (host, held) = RecipientHost::start_with_gate(A2aAdmissionPolicy::Allow, |event| {
        matches!(event, RoomEvent::RecipientItemRetracted { .. })
    })
    .await;
    let daemon = AttachedDaemon::start(vec![host.peer("B")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let item = seed(&mut writer, &host, "seed-detach").await;
    preview(&mut writer, "confirmed", &item).await;
    completion(&daemon, "confirmed").await; // preview, not dispatch completion
    decide(&mut writer, "confirmed", true).await;
    held.wait_for(1).await;
    writer.detach().await;
    held.release_append();
    completion(&daemon, "confirmed").await;
    assert_eq!(held.durable_seen().len(), 1);
    assert_eq!(dispatches(&daemon).await, 1);
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    let mut next = AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    fence(&daemon, &mut next).await;
    assert!(next.frames_for_request("confirmed").is_empty());
    next.detach().await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn k05_queued_confirm_survives_detach_before_its_turn() {
    let host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    // FIFO ordinals: seed send #1, preview #2, decision #3 (held).
    let hooks = TeamSendTestHooks::new(Some(3), None, false);
    let daemon = AttachedDaemon::start_with_test_hooks(vec![host.peer("B")], hooks).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let item = seed(&mut writer, &host, "seed-queued").await;
    assert!(matches!(
        preview(&mut writer, "queued", &item).await,
        TeamRetractPreviewView::Card { armed: true, .. }
    ));
    completion(&daemon, "queued").await; // preview, not dispatch completion
    decide(&mut writer, "queued", true).await;
    daemon.team_send_hooks.wait_for_worker_gate_count(1).await;
    // EOF fence: deregistration completes while the confirm is still queued.
    writer.detach().await;
    assert_eq!(dispatches(&daemon).await, 0);
    daemon.team_send_hooks.release_worker();
    completion(&daemon, "queued").await;
    assert_eq!(dispatches(&daemon).await, 1);
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    assert_eq!(daemon.server.team_retract_slot_count().await, 0);
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn k04_reused_request_key_late_completion_stores_nothing() {
    let host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![host.peer("B")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let older_item = seed(&mut writer, &host, "seed-reuse-older").await;
    let newer_item = seed(&mut writer, &host, "seed-reuse-newer").await;
    let older = daemon.team_send_hooks.hold_retract_preview("reused").await;
    request_preview(&mut writer, "reused", &older_item).await;
    older.wait_entered().await;
    let newer = daemon.team_send_hooks.hold_retract_preview("reused").await;
    request_preview(&mut writer, "reused", &newer_item).await;
    newer.wait_entered().await;
    older.release();
    completion(&daemon, "reused").await;
    newer.release();
    completion(&daemon, "reused").await;
    fence(&daemon, &mut writer).await;
    let previews: Vec<_> = writer
        .frames_for_request("reused")
        .into_iter()
        .filter_map(|frame| match frame {
            DaemonFrame::TeamRetractPreviewed { preview, .. } => Some(preview.clone()),
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            previews.as_slice(),
            [TeamRetractPreviewView::Card { item_id, armed: true, .. }] if item_id == &newer_item
        ),
        "only the newer admission may paint: {previews:?}"
    );
    decide(&mut writer, "reused", true).await;
    answered(&mut writer, "reused").await;
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    let retracted: Vec<_> = read_room_events(&host.journal)
        .await
        .into_iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::RecipientItemRetracted { address, .. } => {
                Some(address.item().as_str().to_owned())
            }
            _ => None,
        })
        .collect();
    assert_eq!(retracted, vec![newer_item]);
    writer.detach().await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn k04_reader_and_peer_retract_frames_refuse_before_io() {
    let host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![host.peer("B")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let item = seed(&mut writer, &host, "seed-authority").await;
    for (tier, sentence) in [
        (ConnectionTier::TrustedLocal, "read-only"),
        (ConnectionTier::Peer, "trusted-local only"),
    ] {
        let mut client = AttachedClient::attach(&daemon.path, tier, true).await;
        request_preview(&mut client, "foreign-preview", &item).await;
        refused(&mut client, "foreign-preview", sentence).await;
        decide(&mut client, "foreign-decision", true).await;
        refused(&mut client, "foreign-decision", sentence).await;
        client.detach().await;
    }
    assert_eq!(dispatches(&daemon).await, 0);
    assert_eq!(host.method_count("x-rustain-items/retract"), 0);
    assert_eq!(host.method_count("x-rustain-items/list"), 0);
    assert!(matches!(
        preview(&mut writer, "writer-control", &item).await,
        TeamRetractPreviewView::Card { armed: true, .. }
    ));
    decide(&mut writer, "writer-control", true).await;
    answered(&mut writer, "writer-control").await;
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    writer.detach().await;
    daemon.stop().await;
    host.stop().await;
}
