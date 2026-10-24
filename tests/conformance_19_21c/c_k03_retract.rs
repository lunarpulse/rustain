//! Story 19.21c K03: shipped card, journal-first dispatch and remembered board.
use super::{
    attached_harness::{AttachedClient, AttachedDaemon, RecipientHost},
    c_k01_thread::{fence, seed},
    harness::read_room_events,
};
use rustain::adapters::{
    a2a::admission::A2aAdmissionPolicy,
    daemon::protocol::{
        ClientFrame, ConnectionTier, DaemonFrame, TeamBoardRequest, TeamRetractPreviewView,
    },
};
use rustain::domain::{
    events::TeamRetractPreview,
    models::{DispatchAct, RoomEvent},
};

pub(super) async fn preview(
    client: &mut AttachedClient,
    key: &str,
    item: &str,
) -> TeamRetractPreviewView {
    client
        .send_frame(ClientFrame::TeamRetractPreview {
            request: key.into(),
            peer: "B".into(),
            item_id: item.into(),
        })
        .await;
    match client.wait_for(|frame| matches!(frame, DaemonFrame::TeamRetractPreviewed { request, .. } if request == key)).await {
        DaemonFrame::TeamRetractPreviewed { preview, .. } => preview,
        _ => unreachable!("matched preview"),
    }
}
pub(super) async fn decide(client: &mut AttachedClient, key: &str, confirm: bool) {
    client
        .send_frame(ClientFrame::TeamRetractDecision {
            request: key.into(),
            confirm,
        })
        .await;
}
pub(super) async fn answered(client: &mut AttachedClient, key: &str) -> String {
    match client.wait_for(|frame| matches!(frame, DaemonFrame::TeamRetractAnswered { request, .. } if request == key)).await {
        DaemonFrame::TeamRetractAnswered { message, .. } => message,
        _ => unreachable!("matched answer"),
    }
}
pub(super) async fn dispatches(daemon: &AttachedDaemon) -> usize {
    read_room_events(&daemon.journal)
        .await
        .iter()
        .filter(|(_, event)| {
            matches!(
                event,
                RoomEvent::RemoteEnvelopeDispatched {
                    act: DispatchAct::ItemRetract { .. },
                    ..
                }
            )
        })
        .count()
}

#[tokio::test]
async fn k03_retract_exact_card_single_post_and_remembered_board_without_list() {
    let (host, append) = RecipientHost::start_with_gate(A2aAdmissionPolicy::Allow, |event| {
        matches!(event, RoomEvent::RecipientItemRetracted { .. })
    })
    .await;
    let daemon = AttachedDaemon::start(vec![host.peer("B")]).await;
    host.observe_retract_dispatches(daemon.journal.node.clone());
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let mut observer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    let item = seed(&mut writer, &host, "seed-retract").await;
    writer
        .team_board("remember", TeamBoardRequest::Peer("B".into()))
        .await;
    let before = writer.wait_board_result("remember").await.unwrap();
    assert!(before.contains(&item));
    let expected = rustain::infrastructure::runtime::transparency_bridge::team_retract_preview(
        daemon.egress.runtime(),
        "B",
        &item,
    )
    .await;
    let actual = preview(&mut writer, "retract", &item).await;
    let TeamRetractPreview::Card(expected) = expected else {
        panic!("oracle card")
    };
    assert_eq!(
        actual,
        TeamRetractPreviewView::Card {
            peer: expected.peer,
            item_id: expected.item_id,
            body: expected.body,
            armed: expected.armed
        }
    );
    let lists = host.method_count("x-rustain-items/list");
    assert!(
        lists >= 3,
        "board, oracle and attached preview use the real list method"
    );
    decide(&mut writer, "retract", true).await;
    append.wait_for(1).await;
    assert_eq!(
        dispatches(&daemon).await,
        1,
        "dispatch is durable before recipient POST append completes"
    );
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    assert_eq!(
        host.dispatches_at_post(),
        vec![1],
        "dispatch exists at the actual POST boundary"
    );
    assert!(append.durable_seen().is_empty());
    append.release_append();
    let board = writer.wait_board_result("retract").await.unwrap();
    assert!(
        board.contains(&item) && board.contains("[retracted "),
        "{board}"
    );
    assert_eq!(
        answered(&mut writer, "retract").await,
        "Retracted on B's host. Marked there — never deleted.\nWhat they already read, they already read."
    );
    assert_eq!(
        host.method_count("x-rustain-items/list"),
        lists,
        "dispatch re-renders, never lists again"
    );
    assert_eq!(host.method_count("x-rustain-items/retract"), 1);
    assert_eq!(dispatches(&daemon).await, 1);
    assert_eq!(append.durable_seen().len(), 1);
    assert!(matches!(preview(&mut writer, "already", &item).await,
        TeamRetractPreviewView::Card { body, .. } if body.contains("already retracted")));
    decide(&mut writer, "already", false).await;
    fence(&daemon, &mut observer).await;
    for key in ["retract", "already"] {
        assert!(observer.frames_for_request(key).is_empty());
    }
    assert!(daemon.requests.lock().is_empty());
    observer.detach().await;
    writer.detach().await;
    daemon.stop().await;
    host.stop().await;
}
