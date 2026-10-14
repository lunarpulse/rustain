//! Story 19.21 K05 — attached-rail send lifecycle keystones.

use crate::attached_harness::{AppendGateJournal, AttachedClient, AttachedDaemon, RecipientHost};
use crate::harness::{Journal, read_room_events};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{AttachMode, ClientFrame, ConnectionTier, DaemonFrame};
use rustain::adapters::daemon::server::{
    MAX_IN_FLIGHT_TEAM_SENDS, TeamSendFifoEventKind, TeamSendPublicationKind, TeamSendTestHooks,
};
use rustain::domain::models::{RoomEvent, SemanticMessageType};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

fn is_team_send_settlement(event: &RoomEvent) -> bool {
    matches!(event, RoomEvent::TeamSendRecipientSettled { .. })
}

fn count_actions(events: &[(u64, RoomEvent)]) -> usize {
    events
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
        .count()
}

fn count_settlements(events: &[(u64, RoomEvent)]) -> usize {
    events
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::TeamSendRecipientSettled { .. }))
        .count()
}

fn count_received(events: &[(u64, RoomEvent)]) -> usize {
    events
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { .. }))
        .count()
}
fn count_dispatches(events: &[(u64, RoomEvent)]) -> usize {
    events
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::RemoteEnvelopeDispatched { .. }))
        .count()
}
fn count_preflight_refusals(events: &[(u64, RoomEvent)]) -> usize {
    events
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::RemoteEnvelopeRejected { .. }))
        .count()
}

fn send_frame(request: &str, recipient: &str, text: &str) -> ClientFrame {
    ClientFrame::TeamSend {
        request: request.to_owned(),
        recipients: vec![recipient.to_owned()],
        text: text.to_owned(),
        message_type: Some(SemanticMessageType::StoryAssignment),
        subject: Some("lifecycle-check".to_owned()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settlement_publication_waits_for_the_durable_append() {
    let recipient = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let journal = Journal::open().await;
    let gate = Arc::new(AppendGateJournal::new(
        journal.room.clone(),
        is_team_send_settlement,
        true,
    ));
    let daemon = AttachedDaemon::start_with_journal(
        journal.clone(),
        vec![recipient.peer("recipient")],
        Some(gate.clone()),
    )
    .await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    client
        .send_frame(send_frame("durable-first", "recipient", "durable body"))
        .await;
    assert!(matches!(
        client.wait_send_started("durable-first").await,
        DaemonFrame::TeamSendStarted { .. }
    ));
    let started = daemon
        .team_send_hooks
        .next_publication()
        .await
        .expect("started publication observation");
    assert_eq!(started.request, "durable-first");
    assert_eq!(started.kind, TeamSendPublicationKind::Started);

    gate.wait_for(1).await;
    assert!(
        matches!(
            daemon.team_send_hooks.try_next_publication().await,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "M38: settlement publication must not precede the held durable append"
    );
    assert_eq!(count_settlements(&read_room_events(&journal).await), 0);

    gate.release_append();
    gate.wait_durable_for(1).await;
    assert!(matches!(
        client.wait_send_settled("durable-first", 0).await,
        DaemonFrame::TeamSendSettled {
            request,
            index: 0,
            ..
        } if request == "durable-first"
    ));
    let settled = daemon
        .team_send_hooks
        .next_publication()
        .await
        .expect("settlement publication observation");
    assert_eq!(settled.request, "durable-first");
    assert_eq!(settled.kind, TeamSendPublicationKind::Settled { index: 0 });
    assert_eq!(count_settlements(&read_room_events(&journal).await), 1);

    daemon.stop().await;
    recipient.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn started_send_survives_detach_and_settlement_targets_its_origin() {
    let recipient = RecipientHost::start_with_gate(A2aAdmissionPolicy::Allow, |event| {
        matches!(event, RoomEvent::RecipientItemReceived { .. })
    })
    .await;
    let (recipient, post_gate) = recipient;
    let journal = Journal::open().await;
    let append_gate = Arc::new(AppendGateJournal::new(
        journal.room.clone(),
        is_team_send_settlement,
        false,
    ));
    let daemon = AttachedDaemon::start_with_journal(
        journal.clone(),
        vec![recipient.peer("recipient")],
        Some(append_gate.clone()),
    )
    .await;
    let mut origin =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    origin
        .send_frame(send_frame(
            "survives-detach",
            "recipient",
            "held recipient POST",
        ))
        .await;
    assert!(matches!(
        origin.wait_send_started("survives-detach").await,
        DaemonFrame::TeamSendStarted { .. }
    ));
    let origin_publication = daemon
        .team_send_hooks
        .next_publication()
        .await
        .expect("started publication observation");
    assert_eq!(origin_publication.kind, TeamSendPublicationKind::Started);
    let (in_flight, started, _, _) = daemon.server.team_send_observations();
    assert_eq!((in_flight, started), (1, 1));
    post_gate.wait_for(1).await;

    drop(origin);
    let mut replacement =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert_eq!(replacement.granted, AttachMode::ReadWrite);
    post_gate.release_append();
    let completion = daemon.team_send_hooks.next_completion().await.unwrap();
    assert_eq!(completion.request, "survives-detach");
    assert_eq!(
        completion.kind,
        rustain::adapters::daemon::server::TeamSendCompletionKind::Finished,
        "M39: started send must finish after its client detaches"
    );
    post_gate.wait_durable_for(1).await;
    append_gate.wait_durable_for(1).await;
    assert_eq!(count_settlements(&read_room_events(&journal).await), 1);

    replacement.team_status("replacement-fence").await;
    assert!(matches!(
        replacement.wait_status_result("replacement-fence").await,
        DaemonFrame::TeamStatusResult { .. }
    ));
    let mut saw_settlement = false;
    loop {
        let publication = daemon
            .team_send_hooks
            .next_publication()
            .await
            .expect("replacement lifecycle publication");
        match publication.request.as_str() {
            "survives-detach" => {
                assert_eq!(
                    publication.kind,
                    TeamSendPublicationKind::Settled { index: 0 }
                );
                assert_eq!(
                    publication.published_to_conn, origin_publication.published_to_conn,
                    "M40: old settlement must stay targeted to its origin"
                );
                saw_settlement = true;
            }
            "replacement-fence" => {
                assert_eq!(publication.kind, TeamSendPublicationKind::StatusResult);
                assert_ne!(
                    publication.published_to_conn,
                    origin_publication.published_to_conn
                );
                break;
            }
            other => panic!("unexpected lifecycle publication {other}"),
        }
    }
    assert!(
        saw_settlement,
        "M40: old settlement must stay targeted to its origin"
    );
    assert!(
        replacement.frames_for_request("survives-detach").is_empty(),
        "M40: replacement must not receive the old writer's settlement"
    );

    daemon.stop().await;
    recipient.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_writer_discards_old_queued_send_but_runs_its_own_send() {
    let recipient = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let hooks = TeamSendTestHooks::new(Some(1), None, false);
    let daemon =
        AttachedDaemon::start_with_test_hooks(vec![recipient.peer("recipient")], hooks).await;
    let mut old_writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    old_writer
        .send_frame(send_frame("old-running", "recipient", "worker-held first"))
        .await;
    let first_enqueued = daemon.team_send_hooks.next_fifo_event().await.unwrap();
    assert_eq!(first_enqueued.request, "old-running");
    assert_eq!(first_enqueued.kind, TeamSendFifoEventKind::Enqueued);
    old_writer
        .send_frame(send_frame("old-queued", "recipient", "must never execute"))
        .await;
    let queued = loop {
        let event = daemon.team_send_hooks.next_fifo_event().await.unwrap();
        if event.request == "old-queued" {
            break event;
        }
    };
    assert_eq!(queued.kind, TeamSendFifoEventKind::Enqueued);

    drop(old_writer);
    let mut replacement =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert_eq!(replacement.granted, AttachMode::ReadWrite);
    daemon.team_send_hooks.release_worker();
    let discarded = loop {
        let event = daemon.team_send_hooks.next_fifo_event().await.unwrap();
        if event.request == "old-queued" {
            break event;
        }
    };
    assert_eq!(
        discarded.kind,
        TeamSendFifoEventKind::Discarded,
        "M46: replacement must discard queued work from the old writer"
    );
    assert_eq!(discarded.connection_id, first_enqueued.connection_id);
    let rows = read_room_events(&daemon.journal).await;
    assert_eq!(count_actions(&rows), 0);
    assert_eq!(count_dispatches(&rows), 0);
    assert_eq!(count_preflight_refusals(&rows), 0);
    // No-POST evidence is the durable-before-I/O invariant: every production
    // outbound POST requires an action record, no preflight refusal, and a
    // RemoteEnvelopeDispatched row first; the recipient's journal stays empty.
    assert_eq!(
        count_received(&read_room_events(&recipient.journal).await),
        0
    );
    replacement
        .send_frame(send_frame(
            "replacement-current",
            "recipient",
            "current writer works",
        ))
        .await;
    assert!(matches!(
        replacement.wait_send_started("replacement-current").await,
        DaemonFrame::TeamSendStarted { .. }
    ));
    assert!(matches!(
        replacement
            .wait_send_settled("replacement-current", 0)
            .await,
        DaemonFrame::TeamSendSettled { .. }
    ));

    let positive_events = read_room_events(&daemon.journal).await;
    assert_eq!(count_actions(&positive_events), 1);
    assert_eq!(count_dispatches(&positive_events), 1);
    assert_eq!(
        count_received(&read_room_events(&recipient.journal).await),
        1
    );

    daemon.stop().await;
    recipient.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_flight_limit_refuses_before_journal_or_http_without_blocking_reads() {
    let (recipient, recipient_gate) =
        RecipientHost::start_with_gate(A2aAdmissionPolicy::Allow, |event| {
            matches!(event, RoomEvent::RecipientItemReceived { .. })
        })
        .await;
    let hooks = TeamSendTestHooks::new(None, None, false).with_send_action_gate();
    let daemon =
        AttachedDaemon::start_with_test_hooks(vec![recipient.peer("recipient")], hooks).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    for index in 0..MAX_IN_FLIGHT_TEAM_SENDS {
        client
            .send_frame(send_frame(
                &format!("saturated-{index}"),
                "recipient",
                "held before coordinator",
            ))
            .await;
    }
    daemon
        .team_send_hooks
        .wait_for_send_action_count(MAX_IN_FLIGHT_TEAM_SENDS)
        .await;
    assert_eq!(
        daemon.server.team_send_observations().0,
        MAX_IN_FLIGHT_TEAM_SENDS,
        "M45: in-flight limit must reserve one slot per admitted send"
    );
    let sender_rows = read_room_events(&daemon.journal).await;
    assert_eq!(count_actions(&sender_rows), 0);
    assert_eq!(count_dispatches(&sender_rows), 0);
    assert_eq!(count_preflight_refusals(&sender_rows), 0);
    // No-POST evidence is the durable-before-I/O invariant, not a raw socket
    // counter: a POST requires an action and dispatch row; no preflight refusal
    // exists, and the recipient's own journal independently records no item.
    assert!(recipient_gate.seen().is_empty());
    assert_eq!(
        count_received(&read_room_events(&recipient.journal).await),
        0
    );

    client
        .send_frame(send_frame("saturation-refusal", "recipient", "must refuse"))
        .await;
    tokio::select! {
        publication = async {
            loop {
                let publication = daemon
                    .team_send_hooks
                    .next_publication()
                    .await
                    .expect("saturation publication observation");
                if publication.request == "saturation-refusal" {
                    break publication;
                }
            }
        } => {
            assert!(matches!(publication.kind, TeamSendPublicationKind::Refused { .. }));
        }
        _ = daemon.team_send_hooks.wait_for_send_action_count(MAX_IN_FLIGHT_TEAM_SENDS + 1) => {
            panic!("M45: saturation request was admitted as an extra in-flight send");
        }
    }
    let refusal = client.wait_send_started("saturation-refusal").await;
    assert!(
        matches!(
            refusal,
            DaemonFrame::RecipientActionResult {
                action: rustain::adapters::daemon::protocol::RecipientAction::TeamSend,
                target: rustain::adapters::daemon::protocol::RecipientActionTarget::Request { request },
                outcome: rustain::adapters::daemon::protocol::RecipientActionOutcome::Refused { .. },
            } if request == "saturation-refusal"
        ),
        "M45: full in-flight capacity must explicitly refuse this request"
    );
    assert_eq!(daemon.server.team_send_observations().3, 1);
    let sender_rows = read_room_events(&daemon.journal).await;
    assert_eq!(count_actions(&sender_rows), 0);
    assert_eq!(count_dispatches(&sender_rows), 0);
    assert_eq!(count_preflight_refusals(&sender_rows), 0);
    assert!(recipient_gate.seen().is_empty());
    assert_eq!(
        count_received(&read_room_events(&recipient.journal).await),
        0
    );

    client.team_status("reader-progress").await;
    assert!(matches!(
        client.wait_status_result("reader-progress").await,
        DaemonFrame::TeamStatusResult { .. }
    ));

    daemon.team_send_hooks.release_send_actions(1);
    recipient_gate.wait_for(1).await;
    let first_positive = read_room_events(&daemon.journal).await;
    assert_eq!(count_actions(&first_positive), 1);
    assert_eq!(count_dispatches(&first_positive), 1);
    recipient_gate.release_append();
    recipient_gate.wait_durable_for(1).await;
    let first_finished = daemon.team_send_hooks.next_completion().await.unwrap();
    assert!(
        first_finished.request.starts_with("saturated-"),
        "the released admitted send is the positive control"
    );
    assert_eq!(
        first_finished.kind,
        rustain::adapters::daemon::server::TeamSendCompletionKind::Finished
    );

    client
        .send_frame(send_frame(
            "admitted-after-release",
            "recipient",
            "positive control",
        ))
        .await;
    daemon
        .team_send_hooks
        .wait_for_send_action_count(MAX_IN_FLIGHT_TEAM_SENDS + 1)
        .await;
    assert_eq!(
        daemon.server.team_send_observations().0,
        MAX_IN_FLIGHT_TEAM_SENDS
    );
    // The shared send-action gate queues released permits fairly. Releasing one
    // slot above admitted this request; open the remaining held actions too.
    daemon
        .team_send_hooks
        .release_send_actions(MAX_IN_FLIGHT_TEAM_SENDS);
    // Release every observed RecipientItemReceived append individually. The
    // admitted request is among the eight completed sends.
    for append_index in 2..=(MAX_IN_FLIGHT_TEAM_SENDS + 1) {
        recipient_gate.wait_for(append_index).await;
        recipient_gate.release_append();
        recipient_gate.wait_durable_for(append_index).await;
    }
    let mut admitted_finished = false;
    while !admitted_finished {
        let completion = daemon.team_send_hooks.next_completion().await.unwrap();
        admitted_finished = completion.request == "admitted-after-release";
        if admitted_finished {
            assert_eq!(
                completion.kind,
                rustain::adapters::daemon::server::TeamSendCompletionKind::Finished
            );
        }
    }
    let positive_events = read_room_events(&daemon.journal).await;
    assert_eq!(
        count_actions(&positive_events),
        MAX_IN_FLIGHT_TEAM_SENDS + 1
    );
    assert_eq!(
        count_dispatches(&positive_events),
        MAX_IN_FLIGHT_TEAM_SENDS + 1
    );
    assert_eq!(
        count_received(&read_room_events(&recipient.journal).await),
        MAX_IN_FLIGHT_TEAM_SENDS + 1
    );

    daemon.stop().await;
    recipient.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_targeted_queue_emits_terminal_error_then_eof_and_never_reroutes() {
    let recipient = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let hooks = TeamSendTestHooks::new(None, Some(1), true);
    let daemon =
        AttachedDaemon::start_with_test_hooks(vec![recipient.peer("recipient")], hooks).await;
    let mut origin =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    origin
        .send_frame(send_frame("queue-full", "recipient", "fill then overflow"))
        .await;
    let started = daemon.team_send_hooks.next_publication().await.unwrap();
    assert_eq!(started.request, "queue-full");
    assert_eq!(started.kind, TeamSendPublicationKind::Started);
    let settled = daemon.team_send_hooks.next_publication().await.unwrap();
    assert_eq!(settled.request, "queue-full");
    assert_eq!(settled.kind, TeamSendPublicationKind::Settled { index: 0 });
    assert_eq!(started.published_to_conn, settled.published_to_conn);
    assert_eq!(daemon.team_send_hooks.overflow_count(), 1);

    let mut other = AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    assert_eq!(other.granted, AttachMode::ReadOnly);
    other.team_status("other-fence").await;
    assert!(matches!(
        other.wait_status_result("other-fence").await,
        DaemonFrame::TeamStatusResult { .. }
    ));
    let other_publication = daemon.team_send_hooks.next_publication().await.unwrap();
    assert_eq!(other_publication.request, "other-fence");
    assert_ne!(
        other_publication.published_to_conn, started.published_to_conn,
        "a different connection must own its own targeted status"
    );
    daemon.team_send_hooks.release_writer();
    origin
        .stream
        .shutdown()
        .await
        .expect("half-close origin writer");
    let terminal_frame = tokio::time::timeout(
        crate::attached_harness::FIXTURE_TIMEOUT,
        rustain::adapters::daemon::protocol::read_frame::<_, DaemonFrame>(&mut origin.stream),
    )
    .await
    .expect("terminal frame deadline")
    .expect("decode terminal frame");
    assert!(
        matches!(
            terminal_frame,
            Some(DaemonFrame::Error(
                rustain::adapters::daemon::protocol::ProtocolError::Internal(message)
            )) if message.contains("team result queue overflow")
        ),
        "M41: full targeted output must close with its explicit terminal error"
    );
    let terminal_event = daemon
        .team_send_hooks
        .next_terminal_event()
        .await
        .expect("terminal error observation");
    assert_eq!(terminal_event.connection_id, started.published_to_conn);
    assert!(
        matches!(
            terminal_event.kind,
            rustain::adapters::daemon::server::TeamSendTerminalEventKind::Error(_)
        ),
        "M41: full targeted output must close with its explicit terminal error"
    );
    let eof = tokio::time::timeout(
        crate::attached_harness::FIXTURE_TIMEOUT,
        rustain::adapters::daemon::protocol::read_frame::<_, DaemonFrame>(&mut origin.stream),
    )
    .await
    .expect("terminal EOF deadline")
    .expect("valid terminal EOF");
    assert!(eof.is_none(), "M41: terminal Error must be followed by EOF");
    assert!(origin.frames_for_request("queue-full").is_empty());
    assert!(other.frames_for_request("queue-full").is_empty());
    assert_eq!(daemon.team_send_hooks.overflow_count(), 1);

    daemon.stop().await;
    recipient.stop().await;

    // Unbounded-by-default control: this same targeted Started/Settled result
    // pair reaches its requesting connection when the queue has ample room.
    let recipient = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![recipient.peer("recipient")]).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    client
        .send_frame(send_frame("queue-control", "recipient", "ample capacity"))
        .await;
    assert!(matches!(
        client.wait_send_started("queue-control").await,
        DaemonFrame::TeamSendStarted { .. }
    ));
    assert!(matches!(
        client.wait_send_settled("queue-control", 0).await,
        DaemonFrame::TeamSendSettled { .. }
    ));
    daemon.stop().await;
    recipient.stop().await;
}
