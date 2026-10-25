//! Story 19.21 R01 — sender-local action facts stay off raw Peer-tier wire events.
use super::attached_harness::{AttachedClient, AttachedDaemon, RecipientHost};
use super::harness::read_room_events;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{AttachMode, ConnectionTier, DaemonFrame};
use rustain::domain::models::{Authorship, RoomEvent};
use rustain::infrastructure::runtime::event_bus::{RawEvent, RawEventKind};

#[tokio::test]
async fn peer_raw_events_omit_sender_action_facts_but_keep_sanitized_legacy_dispatch() {
    let recipient = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![recipient.peer("recipient-private")]).await;
    let mut peer = AttachedClient::attach(&daemon.path, ConnectionTier::Peer, false).await;
    assert_eq!(peer.granted, AttachMode::ReadOnly);
    let mut trusted =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    trusted
        .send_team(
            "private-action-request",
            vec!["recipient-private".into()],
            "sender body is not a wire projection",
            None,
            Some("sender-only-subject-7f2d".into()),
            None,
            None,
        )
        .await;
    assert!(matches!(
        trusted.wait_send_started("private-action-request").await,
        DaemonFrame::TeamSendStarted { .. }
    ));
    assert!(matches!(
        trusted.wait_send_settled("private-action-request", 0).await,
        DaemonFrame::TeamSendSettled { .. }
    ));

    let durable = read_room_events(&daemon.journal).await;
    let action = durable
        .iter()
        .find_map(|(_, event)| match event {
            RoomEvent::TeamSendActionRecorded {
                action,
                subject: Some(subject),
                recipients,
                ..
            } if subject == "sender-only-subject-7f2d" => {
                assert_eq!(recipients.len(), 1);
                Some(action.clone())
            }
            _ => None,
        })
        .expect("sender journal retains the private action record");
    assert!(
        durable.iter().any(|(_, event)| matches!(
            event,
            RoomEvent::TeamSendRecipientSettled { action: found, index: 0, .. }
                if found == &action
        )),
        "sender journal retains the typed recipient settlement"
    );
    let dispatch = durable
        .iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RemoteEnvelopeDispatched {
                peer,
                task,
                bytes,
                act,
                content_hash,
                endpoint,
                action: Some(action_ref),
                authorship: Authorship::HumanWritten,
                ..
            } if action_ref.action == action => Some((
                peer.clone(),
                task.clone(),
                *bytes,
                act.clone(),
                *content_hash,
                endpoint.clone(),
                action_ref.clone(),
            )),
            _ => None,
        })
        .expect("legacy dispatch journal row retains its action correlation");

    // A journal sentinel shares the send facts' exact FIFO domain channel.
    // Unlike provider completion, it cannot overtake queued room events.
    daemon
        .room_journal
        .record_event(RoomEvent::Unrecognized)
        .await
        .expect("append the domain-stream fence");
    tokio::time::timeout(super::attached_harness::FIXTURE_TIMEOUT, async {
        loop {
            let frame = peer.collect_one().await;
            if matches!(
                frame,
                DaemonFrame::Event(RawEvent {
                    kind: RawEventKind::DomainEvent(
                        rustain::domain::events::DomainEventPayload::Room(RoomEvent::Unrecognized)
                    ),
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .expect("Peer receives the domain-stream positive-control fence");

    let event_frames: Vec<_> = peer
        .received
        .iter()
        .filter_map(|frame| match frame {
            DaemonFrame::Event(event) => Some(event),
            _ => None,
        })
        .collect();
    assert!(
        !event_frames.iter().any(|event| matches!(
            &event.kind,
            RawEventKind::DomainEvent(rustain::domain::events::DomainEventPayload::Room(
                RoomEvent::TeamSendActionRecorded { .. }
                    | RoomEvent::TeamSendRecipientSettled { .. }
            ))
        )),
        "Peer raw event projection contains no sender-local action or settlement variant"
    );
    let wire_dispatch = event_frames
        .iter()
        .find_map(|event| match &event.kind {
            RawEventKind::DomainEvent(rustain::domain::events::DomainEventPayload::Room(
                RoomEvent::RemoteEnvelopeDispatched {
                    peer,
                    task,
                    bytes,
                    act,
                    content_hash,
                    endpoint,
                    action,
                    ..
                },
            )) => Some((peer, task, bytes, act, content_hash, endpoint, action)),
            _ => None,
        })
        .expect("Peer still receives the legacy dispatch positive control");
    assert_eq!(wire_dispatch.0, &dispatch.0);
    assert_eq!(wire_dispatch.1, &dispatch.1);
    assert_eq!(*wire_dispatch.2, dispatch.2);
    assert_eq!(wire_dispatch.3, &dispatch.3);
    assert_eq!(wire_dispatch.4, &dispatch.4);
    assert_eq!(wire_dispatch.5, &dispatch.5);
    assert!(
        wire_dispatch.6.is_none(),
        "legacy event action reference is sanitized"
    );
    let raw_wire = serde_json::to_string(&event_frames).expect("raw events serialize");
    assert!(
        !raw_wire.contains("\"authorship\""),
        "sender-side dispatch provenance stays journal-local"
    );
    assert!(!raw_wire.contains("sender-only-subject-7f2d"));
    assert!(
        !raw_wire.contains(action.as_str()),
        "action id stays sender-local"
    );

    daemon.stop().await;
    recipient.stop().await;
}
