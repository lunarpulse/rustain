use rustain::domain::clock::Clock;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::harness::{
    GateRoomJournal, Host, Journal, RecordingRoomJournal, RejectAppend, Sender, folded_actions,
    read_room_events,
};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::board::{
    ActionSelector, board_refresh_floor, collect_action_board, render_action_board,
};
use rustain::adapters::a2a::send::{SendRequest, coordinate_send};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{ResponseMode, RoomEvent};
use rustain::domain::ports::RoomJournal;
use tokio_util::sync::CancellationToken;

fn second_recipient_dispatch(event: &RoomEvent) -> bool {
    matches!(
        event,
        RoomEvent::RemoteEnvelopeDispatched {
            action: Some(reference),
            ..
        } if reference.index == 1
    )
}

fn settlement(event: &RoomEvent) -> bool {
    matches!(event, RoomEvent::TeamSendRecipientSettled { .. })
}

async fn action_board_frontdoor(sender: &mut Sender, state: &mut TuiState, action: &str) -> String {
    sender
        .command(state, &format!("/team board --action {action}"))
        .await;
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            match sender.domain_rx.recv().await {
                Some(AppEvent::TeamBoardReady {
                    conversation_id,
                    message,
                }) if conversation_id == "conv" => return message,
                Some(_) => {}
                None => panic!("domain event stream closed before action board"),
            }
        }
    })
    .await
    .expect("production action board response")
}

async fn action_board_at(sender: &Sender, action: &str, now: Instant) -> String {
    let view = collect_action_board(
        sender.runtime(),
        &ActionSelector::Id(action.to_owned()),
        now,
    )
    .await
    .expect("synthetic-time action board collection");
    render_action_board(&view, sender.clock.wall_now_ms() / 1_000)
}

/// **K03b L1.** M57-stale-entry-live and M57-token-released-before-delivery → RED
/// when the cancelled action remains `sending…` or a held action is not live.
/// Positive control: the independent same-runtime coordinator slot stays live
/// while its real recipient append is held. The initial observation enters
/// through `/team board --action`; later observations call that front door's
/// public collector with increasing synthetic `Instant`s to honor its 500 ms
/// design floor without sleeping or weakening the production guard.
#[tokio::test]
async fn k03b_cancelled_front_door_send_releases_only_its_live_slot() {
    let cancelled_journal = Journal::open().await;
    let cancelled_gate = Arc::new(GateRoomJournal::new(
        cancelled_journal.room.clone(),
        |event| matches!(event, RoomEvent::RecipientItemReceived { .. }),
    ));
    let cancelled_sink: Arc<dyn RoomJournal> = cancelled_gate.clone();
    let cancelled_host = Host::start_with_journal(
        cancelled_journal,
        cancelled_sink,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;

    let control_journal = Journal::open().await;
    let control_gate = Arc::new(GateRoomJournal::new(
        control_journal.room.clone(),
        |event| matches!(event, RoomEvent::RecipientItemReceived { .. }),
    ));
    let control_sink: Arc<dyn RoomJournal> = control_gate.clone();
    let control_host = Host::start_with_journal(
        control_journal,
        control_sink,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let front_door_host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;

    let mut sender = Sender::start(vec![
        cancelled_host.peer("cancelled"),
        control_host.peer("control"),
        front_door_host.peer("front-door"),
    ])
    .await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(
            &mut state,
            "/team send front-door front-door-positive-control",
        )
        .await;
    sender.settle(&mut state, 1).await;

    let runtime = sender.runtime().clone();
    let cancel = CancellationToken::new();
    let cancel_for_send = cancel.clone();
    let cancelled_alias = "cancelled".to_owned();
    let cancelled_send = tokio::spawn(async move {
        coordinate_send(
            &runtime,
            SendRequest {
                recipients: &[cancelled_alias],
                text: "cancel before recipient append",
                message_type: None,
                reply_to: None,
                subject: None,
                stories: None,
                selection: None,
                authorship: rustain::domain::models::Authorship::HumanWritten,
                send_authorization: rustain::domain::models::SendAuthorization::HumanDecided,
            },
            cancel_for_send,
            |_| {},
            |_, _| {},
        )
        .await
    });
    cancelled_gate.wait_started().await;
    let cancelled_action = folded_actions(&sender.journal)
        .await
        .last()
        .expect("cancelled action was recorded")
        .id
        .as_str()
        .to_owned();

    let runtime = sender.runtime().clone();
    let control_alias = "control".to_owned();
    let control_send = tokio::spawn(async move {
        coordinate_send(
            &runtime,
            SendRequest {
                recipients: &[control_alias],
                text: "held control action",
                message_type: None,
                reply_to: None,
                subject: None,
                stories: None,
                selection: None,
                authorship: rustain::domain::models::Authorship::HumanWritten,
                send_authorization: rustain::domain::models::SendAuthorization::HumanDecided,
            },
            CancellationToken::new(),
            |_| {},
            |_, _| {},
        )
        .await
    });
    control_gate.wait_started().await;
    let control_action = folded_actions(&sender.journal)
        .await
        .last()
        .expect("control action was recorded")
        .id
        .as_str()
        .to_owned();

    let held = action_board_frontdoor(&mut sender, &mut state, &cancelled_action).await;
    assert!(
        held.contains("cancelled") && held.contains("sending…"),
        "a real recipient append held before landing is live:\n{held}"
    );

    cancel.cancel();
    cancelled_send
        .await
        .expect("cancelled coordinator task")
        .expect("cancelled send exits normally");
    let floor = board_refresh_floor();
    let mut board_now = Instant::now() + floor;
    let cancelled = action_board_at(&sender, &cancelled_action, board_now).await;
    assert!(
        !cancelled.contains("sending…"),
        "cancelled front-door send retained a live marker:\n{cancelled}"
    );
    assert!(
        cancelled.contains("cancelled") && cancelled.contains("no outcome recorded"),
        "{cancelled}"
    );
    board_now += floor;
    let control = action_board_at(&sender, &control_action, board_now).await;
    assert!(
        control.contains("control") && control.contains("sending…"),
        "the independent still-held control action must remain live:\n{control}"
    );

    cancelled_gate.release();
    control_gate.release();
    control_send
        .await
        .expect("control coordinator task")
        .expect("control send completes");
    cancelled_host.stop().await;
    control_host.stop().await;
    front_door_host.stop().await;
}

/// **K03b L2/L3.** M57-shared-action-token → RED when recipient 0 remains
/// `sending…` after its settlement append fails; M57-token-released-before-delivery
/// → RED when the held recipient 1 loses `sending…` before its append is released.
/// L3 also kills stale completion markers after both sends finish. The first
/// observation enters through `/team board --action`; the final one uses the
/// same collector with a synthetic later `Instant` to honor its design floor.
/// Positive controls: recipient 1 is live while held and later renders delivered;
/// recipient 0's real policy rejection left its list empty.
#[tokio::test]
async fn k03b_settlement_failure_is_slot_local_and_finished_rows_are_not_live() {
    let first = Host::start(A2aAdmissionPolicy::Deny, ResponseMode::NotifyAndWait).await;
    let second = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let journal = Journal::open().await;
    let recorder = Arc::new(RecordingRoomJournal::new(
        journal.room.clone(),
        Some(RejectAppend::new(settlement, 0)),
    ));
    let gate = Arc::new(GateRoomJournal::new(
        recorder.clone(),
        second_recipient_dispatch,
    ));
    let sink: Arc<dyn RoomJournal> = gate.clone();
    let mut sender = Sender::with_journal(
        vec![first.peer("first"), second.peer("second")],
        sink,
        journal,
    )
    .await;
    let mut state = TuiState::new(100, 40);

    sender
        .command(&mut state, "/team send first,second slot-liveness")
        .await;
    recorder.wait_for(settlement).await;
    gate.wait_started().await;
    let action = folded_actions(&sender.journal)
        .await
        .last()
        .expect("recorded send")
        .id
        .as_str()
        .to_owned();
    let board = action_board_frontdoor(&mut sender, &mut state, &action).await;
    assert!(
        board.contains("first") && board.contains("no outcome recorded"),
        "settlement-failed slot must no longer be live:\n{board}"
    );
    assert!(
        board.contains("second") && board.contains("sending…"),
        "held independent slot must remain live:\n{board}"
    );
    assert!(
        read_room_events(&first.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RemoteEnvelopeRejected { .. })),
        "the rejected settlement follows a real recipient policy response"
    );

    gate.release();
    sender.settle(&mut state, 1).await;
    let finished = action_board_at(&sender, &action, Instant::now() + board_refresh_floor()).await;
    assert!(
        finished.contains("second") && finished.contains("delivered"),
        "a landed POST with durable settlement is the delivered positive control:\n{finished}"
    );
    assert!(
        !finished.contains("sending…"),
        "a completed action must not retain any live row:\n{finished}"
    );
    let events = read_room_events(&sender.journal).await;
    assert_eq!(
        events
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::TeamSendRecipientSettled { .. }))
            .count(),
        1,
        "only recipient 1 has a durable settlement"
    );
    first.stop().await;
    second.stop().await;
}
