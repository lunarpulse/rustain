use std::sync::Arc;

use super::harness::{
    GateRoomJournal, Host, Journal, RecordingRoomJournal, RejectAppend, Sender, folded_actions,
    read_room_events,
};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::send::{SendRequest, coordinate_send};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::ResponseMode;
use rustain::domain::models::{DispatchAct, RoomEvent, SendActionId};
use tokio_util::sync::CancellationToken;

fn is_action_record(event: &RoomEvent) -> bool {
    matches!(event, RoomEvent::TeamSendActionRecorded { .. })
}
fn is_recipient_settlement(event: &RoomEvent) -> bool {
    matches!(event, RoomEvent::TeamSendRecipientSettled { .. })
}
fn is_retract_dispatch(event: &RoomEvent) -> bool {
    matches!(
        event,
        RoomEvent::RemoteEnvelopeDispatched {
            act: DispatchAct::ItemRetract { .. },
            ..
        }
    )
}
fn is_accepted(event: &RoomEvent) -> bool {
    matches!(event, RoomEvent::RemoteEnvelopeAccepted { .. })
}
fn is_task_dispatch(event: &RoomEvent) -> bool {
    matches!(
        event,
        RoomEvent::RemoteEnvelopeDispatched {
            act: DispatchAct::Task,
            ..
        }
    )
}

/// **K01 ordering/privacy leg. Mutants M01 (action record after first recipient
/// I/O), M02 (record only settled recipients), M03 (persist body/body-derived
/// label), M05 (dispatch row drops action/index), M06 (visible settle before
/// durable settlement), M11 (send label on wire) → RED.** Positive control:
/// all three real hosts receive the body, record their independently correlated
/// dispatch/settlement, and publish their visible settlement.
#[tokio::test]
async fn typed_labelled_send_records_action_before_all_real_recipient_io() {
    let a = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let b = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let c = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![a.peer("a"), b.peer("b"), c.peer("c")]).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(
            &mut state,
            "/team send --type story_assignment --subject sprint-7 a,b,c private-body-419",
        )
        .await;
    sender.settle(&mut state, 3).await;

    let rows = read_room_events(&sender.journal).await;
    let action_pos = rows
        .iter()
        .position(|(_, e)| matches!(e, RoomEvent::TeamSendActionRecorded { .. }))
        .expect("action record");
    let action_seq = rows[action_pos].0;
    let (action, recipients, subject, body_bytes) = match &rows[action_pos].1 {
        RoomEvent::TeamSendActionRecorded {
            action,
            recipients,
            subject,
            body_bytes,
            ..
        } => (action.clone(), recipients, subject.as_deref(), *body_bytes),
        _ => unreachable!(),
    };
    assert_eq!(
        recipients
            .iter()
            .map(|r| r.alias.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    assert_eq!(subject, Some("sprint-7"));
    assert_eq!(body_bytes, "private-body-419".len());
    let dispatches: Vec<_> = rows
        .iter()
        .filter_map(|(seq, event)| match event {
            RoomEvent::RemoteEnvelopeDispatched {
                action: Some(reference),
                ..
            } if reference.action == action => Some((*seq, reference.index)),
            _ => None,
        })
        .collect();
    let settlements: Vec<_> = rows
        .iter()
        .filter_map(|(seq, event)| match event {
            RoomEvent::TeamSendRecipientSettled {
                action: id, index, ..
            } if id == &action => Some((*seq, *index)),
            _ => None,
        })
        .collect();
    // The recipients are independent and run concurrently: which dispatch or
    // settlement lands first is not part of the contract, the SET of indices
    // is (operator order is asserted on the action record above).
    let sorted = |rows: &[(u64, u32)]| {
        let mut indices: Vec<u32> = rows.iter().map(|(_, i)| *i).collect();
        indices.sort_unstable();
        indices
    };
    assert_eq!(sorted(&dispatches), [0, 1, 2]);
    assert_eq!(sorted(&settlements), [0, 1, 2]);
    assert!(dispatches.iter().all(|(seq, _)| *seq > action_seq));
    assert!(settlements.iter().all(|(seq, _)| *seq > action_seq));
    // Nothing the send path emits may precede the action record — not only the
    // rows that carry its reference: a defective ordering that performs recipient
    // I/O first writes its rows (referenced or not) BEFORE the record.
    // **M01 (record after the first recipient I/O) → RED** on this assertion.
    assert!(
        rows.iter().all(|(seq, event)| *seq > action_seq
            || !matches!(
                event,
                RoomEvent::RemoteEnvelopeDispatched { .. }
                    | RoomEvent::RemoteEnvelopeAccepted { .. }
                    | RoomEvent::RemoteEnvelopeRejected { .. }
                    | RoomEvent::TeamSendRecipientSettled { .. }
            )),
        "no recipient preflight, dispatch, outcome or settlement row precedes the action record"
    );
    assert_eq!(
        rows.iter()
            .filter(|(_, e)| matches!(e, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        1
    );
    let non_action_rows = rows
        .iter()
        .filter(|(_, event)| !matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
        .map(|(_, event)| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!non_action_rows.contains("private-body-419"));
    assert!(
        !non_action_rows.contains("sprint-7"),
        "label remains confined to the action's subject field"
    );
    for host in [&a, &b, &c] {
        let received = read_room_events(&host.journal).await;
        let text = received
            .iter()
            .filter_map(|(_, event)| match event {
                RoomEvent::RecipientItemReceived { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("private-body-419"),
            "the body reached the recipient"
        );
        assert!(
            !text.contains("sprint-7"),
            "the sender-local label did not reach the recipient"
        );
    }
    a.stop().await;
    b.stop().await;
    c.stop().await;
}

/// **K01 outcome leg. Mutants M02 (omit recipients without a usable answer),
/// M05 (drop `{action,index}`) → RED.** Positive control: accepted, ask-policy
/// and an unproven first answer settle as three explicit per-recipient variants.
#[tokio::test]
async fn one_action_captures_delivered_auth_required_and_unproven_answer() {
    let delivered = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let awaiting = Host::start(A2aAdmissionPolicy::Ask, ResponseMode::NotifyAndWait).await;
    let failed_journal = Journal::open().await;
    let failed_sink = Arc::new(RecordingRoomJournal::new(
        failed_journal.room.clone(),
        Some(RejectAppend::new(is_accepted, 0)),
    ));
    let failed_sink_dyn: Arc<dyn rustain::domain::ports::RoomJournal> = failed_sink;
    let unproven = Host::start_with_journal(
        failed_journal,
        failed_sink_dyn,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let mut sender = Sender::start(vec![
        delivered.peer("delivered"),
        awaiting.peer("awaiting"),
        unproven.peer("unproven"),
    ])
    .await;
    let mut state = TuiState::new(100, 40);
    sender.command(&mut state, "/team send --type story_assignment --subject outcomes delivered,awaiting,unproven typed-outcome-body").await;
    sender.settle(&mut state, 3).await;
    let rows = read_room_events(&sender.journal).await;
    let (action, recipients) = rows
        .iter()
        .find_map(|(_, event)| match event {
            RoomEvent::TeamSendActionRecorded {
                action, recipients, ..
            } => Some((action, recipients)),
            _ => None,
        })
        .expect("one whole-action record");
    assert_eq!(
        recipients
            .iter()
            .map(|r| r.alias.as_str())
            .collect::<Vec<_>>(),
        ["delivered", "awaiting", "unproven"]
    );
    let mut settlements: Vec<_> = rows
        .iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::TeamSendRecipientSettled {
                action: id,
                index,
                settlement,
            } if id == action => Some((*index, settlement)),
            _ => None,
        })
        .collect();
    settlements.sort_by_key(|(index, _)| *index);
    assert_eq!(settlements.len(), 3);
    assert!(matches!(
        settlements[0].1,
        rustain::domain::models::SendSettlement::Delivered { .. }
    ));
    assert!(matches!(
        settlements[1].1,
        rustain::domain::models::SendSettlement::AwaitingApproval
    ));
    assert!(matches!(
        settlements[2].1,
        rustain::domain::models::SendSettlement::NoUsableAnswer
    ));
    delivered.stop().await;
    awaiting.stop().await;
    unproven.stop().await;
}

/// **K01 preflight leg. Mutant M02 (action contains only recipients that
/// dispatch) → RED.** Positive control: the same real host accepts a send when
/// the missing credential is not configured.
#[tokio::test]
async fn preflight_refusal_is_action_member_without_dispatch() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut refused_peer = host.peer("preflight");
    refused_peer.auth = Some("RUSTAIN_19_21_INTENTIONALLY_UNSET_CREDENTIAL".to_owned());
    let mut sender = Sender::start(vec![refused_peer]).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send preflight body-preflight")
        .await;
    sender.settle(&mut state, 1).await;
    let rows = read_room_events(&sender.journal).await;
    let action_pos = rows
        .iter()
        .position(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
        .expect("action before preflight");
    let action = match &rows[action_pos].1 {
        RoomEvent::TeamSendActionRecorded {
            action, recipients, ..
        } => {
            assert_eq!(recipients.len(), 1);
            action
        }
        _ => unreachable!(),
    };
    assert!(rows.iter().any(|(_, event)| matches!(event, RoomEvent::TeamSendRecipientSettled { action: id, settlement: rustain::domain::models::SendSettlement::Unreachable { .. }, .. } if id == action)));
    assert!(!rows.iter().any(|(_, event)| matches!(event, RoomEvent::RemoteEnvelopeDispatched { action: Some(reference), .. } if &reference.action == action)));
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemReceived { .. }))
    );
    let mut control = Sender::start(vec![host.peer("preflight-control")]).await;
    let mut control_state = TuiState::new(100, 40);
    control
        .command(
            &mut control_state,
            "/team send preflight-control body-control",
        )
        .await;
    control.settle(&mut control_state, 1).await;
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { .. }))
    );
    host.stop().await;
}

/// **K01 append-failure leg (i). Mutant M07 (continue recipient I/O after a
/// failed action append) → RED.** Positive control: with appends enabled the
/// same recipient receives and journals the control POST.
#[tokio::test]
async fn failed_action_append_is_all_not_sent_and_never_touches_recipient() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let journal = Journal::open().await;
    let recorder = Arc::new(RecordingRoomJournal::new(
        journal.room.clone(),
        Some(RejectAppend::new(is_action_record, 0)),
    ));
    let sink: Arc<dyn rustain::domain::ports::RoomJournal> = recorder.clone();
    let mut sender = Sender::with_journal(vec![host.peer("only")], sink, journal).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send only private-body-action-fail")
        .await;
    recorder.wait_for(is_action_record).await;
    sender.settle(&mut state, 1).await;
    assert_eq!(recorder.observed.lock().len(), 1);
    assert!(sender.rows().await.iter().all(|entry| !matches!(
        entry.record,
        rustain::domain::models::JournalRecord::Room(
            RoomEvent::RemoteEnvelopeDispatched { .. } | RoomEvent::TeamSendRecipientSettled { .. }
        )
    )));
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemReceived { .. }))
    );
    assert!(
        state.feedback_blocks["team-send-1"]
            .message
            .contains("not sent")
    );
    let mut healthy = Sender::start(vec![host.peer("control")]).await;
    let mut healthy_state = TuiState::new(100, 40);
    healthy
        .command(
            &mut healthy_state,
            "/team send control positive-control-body",
        )
        .await;
    healthy.settle(&mut healthy_state, 1).await;
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { .. }))
    );
    host.stop().await;
}

/// **K01 append-failure leg (ii). Mutant M09 (rewrite a landed POST as
/// `NotSent` or publish an undurable settle) → RED.** Positive control: with
/// appends enabled, the same host receives and durably settles another send.
#[tokio::test]
async fn failed_settlement_append_latches_notice_without_not_sent_claim_or_settle() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let journal = Journal::open().await;
    let recorder = Arc::new(RecordingRoomJournal::new(
        journal.room.clone(),
        Some(RejectAppend::new(is_recipient_settlement, 0)),
    ));
    let sink: Arc<dyn rustain::domain::ports::RoomJournal> = recorder.clone();
    let mut sender = Sender::with_journal(vec![host.peer("peer")], sink, journal).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send peer landed-before-settlement-fails")
        .await;
    recorder.wait_for(is_recipient_settlement).await;
    let (failures, detail) = tokio::time::timeout(std::time::Duration::from_secs(45), async {
        loop {
            match sender.egress_rx.recv().await {
                Some(rustain::domain::events::AppEvent::DomainEvent(
                    rustain::domain::events::DomainEventPayload::TransparencyJournalFailed {
                        failures,
                        detail,
                    },
                )) => break (failures, detail),
                Some(_) => {}
                None => panic!("egress journal-failure stream closed"),
            }
        }
    })
    .await
    .expect("journal failure is latched visibly");
    assert_eq!(failures, 1);
    assert!(detail.contains("injected targeted append refusal"));
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { .. })),
        "the remote POST landed"
    );
    let rows = read_room_events(&sender.journal).await;
    assert!(
        rows.iter()
            .any(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
    );
    assert!(
        rows.iter()
            .any(|(_, event)| matches!(event, RoomEvent::RemoteEnvelopeDispatched { .. }))
    );
    assert!(
        !rows
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::TeamSendRecipientSettled { .. }))
    );
    assert!(
        !state.feedback_blocks["team-send-1"]
            .message
            .contains("not sent")
    );
    assert!(
        matches!(
            sender.domain_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "no undurable settle was published"
    );
    let mut healthy = Sender::start(vec![host.peer("healthy")]).await;
    let mut healthy_state = TuiState::new(100, 40);
    healthy
        .command(
            &mut healthy_state,
            "/team send healthy append-enabled-control",
        )
        .await;
    healthy.settle(&mut healthy_state, 1).await;
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { .. }))
            .count()
            >= 2
    );
    host.stop().await;
}

/// **K01 append-failure leg (iii), A15. Mutant M10 (send retract POST after
/// dispatch append fails) → RED.** Positive control: the same recipient/item
/// retracts when its sender journal can append the dispatch record.
#[tokio::test]
async fn failed_retract_dispatch_append_prevents_post_and_renders_a15_refusal() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let journal = Journal::open().await;
    let recorder = Arc::new(RecordingRoomJournal::new(
        journal.room.clone(),
        Some(RejectAppend::new(is_retract_dispatch, 0)),
    ));
    let sink: Arc<dyn rustain::domain::ports::RoomJournal> = recorder.clone();
    let mut sender = Sender::with_journal(vec![host.peer("peer")], sink, journal).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send peer seed-a15-item")
        .await;
    sender.settle(&mut state, 1).await;
    let item_id = read_room_events(&host.journal)
        .await
        .iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { address, .. } => {
                Some(address.item().as_str().to_owned())
            }
            _ => None,
        })
        .expect("real inbound item to retract");
    sender
        .raise_retract_card(&mut state, &format!("/team retract peer {item_id}"))
        .await;
    assert!(
        state
            .pending_team_retract
            .as_ref()
            .is_some_and(|card| card.armed),
        "retract card is not armed: {:?}",
        state.pending_team_retract
    );
    let refusal = sender
        .confirm_retract(&mut state)
        .await
        .expect("armed card confirms");
    assert_eq!(refusal, "not sent — this host could not record the attempt");
    assert!(
        recorder.observed.lock().iter().any(is_retract_dispatch),
        "the targeted append gate fired"
    );
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemRetracted { .. }))
    );
    assert!(sender.rows().await.iter().all(|entry| !matches!(
        entry.record,
        rustain::domain::models::JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched {
            act: DispatchAct::ItemRetract { .. },
            ..
        })
    )));

    let mut control = Sender::start(vec![host.peer("peer")]).await;
    let mut control_state = TuiState::new(100, 40);
    control
        .command(
            &mut control_state,
            "/team send peer seed-positive-control-item",
        )
        .await;
    control.settle(&mut control_state, 1).await;
    let item_id = read_room_events(&host.journal)
        .await
        .iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { address, .. } => {
                Some(address.item().as_str().to_owned())
            }
            _ => None,
        })
        .nth(1)
        .expect("positive control inbound item");
    control
        .raise_retract_card(&mut control_state, &format!("/team retract peer {item_id}"))
        .await;
    let landed = control
        .confirm_retract(&mut control_state)
        .await
        .expect("positive control retracts");
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RecipientItemRetracted { .. })),
        "an append-enabled control must reach the same real recipient; response was {landed:?}",
    );
    host.stop().await;
}

/// **K01 follow-up leg. Mutant M04 (a follow-up records a new send action) →
/// RED.** Positive control: the follow-up's real dispatch and settlement finish,
/// while the action fold remains exactly one recorded operator send.
#[tokio::test]
async fn followup_uses_delivery_path_without_minting_a_send_action() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![host.peer("peer")]).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send peer original-body")
        .await;
    sender.settle(&mut state, 1).await;
    let first_rows = read_room_events(&sender.journal).await;
    let turn_id = first_rows
        .iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task), ..
            } => Some(task.clone()),
            _ => None,
        })
        .expect("send dispatch task id");
    assert_eq!(folded_actions(&sender.journal).await.len(), 1);
    sender
        .command(
            &mut state,
            &format!("/team followup peer {turn_id} followup-body"),
        )
        .await;
    sender.settle(&mut state, 1).await;
    assert_eq!(folded_actions(&sender.journal).await.len(), 1);
    assert!(
        read_room_events(&sender.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(
                event,
                RoomEvent::RemoteEnvelopeDispatched { action: None, .. }
            ))
    );
    host.stop().await;
}

/// **K01 membership/restart leg. Mutant M08 (join by alias or newest dispatch
/// instead of action/index) → RED.** Positive control: after reopening the real
/// NodeJournal in a fresh runtime, both same-alias actions retain independent
/// outcomes, including the later preflight refusal with no dispatch.
#[tokio::test]
async fn same_alias_actions_reconstruct_independent_outcomes_after_restart() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let journal = Journal::open().await;
    let mut first = Sender::with_journal(
        vec![host.peer("shared")],
        journal.room.clone(),
        journal.clone(),
    )
    .await;
    let mut first_state = TuiState::new(100, 40);
    first
        .command(&mut first_state, "/team send shared first-action-body")
        .await;
    first.settle(&mut first_state, 1).await;
    let first_action = folded_actions(&first.journal).await[0].id.clone();
    drop(first);

    let reopened = journal.reopen().await;
    let mut refused_peer = host.peer("shared");
    refused_peer.auth = Some("RUSTAIN_19_21_RESTART_INTENTIONALLY_UNSET".to_owned());
    let mut fresh = Sender::with_journal(vec![refused_peer], reopened.room.clone(), reopened).await;
    let mut fresh_state = TuiState::new(100, 40);
    fresh
        .command(&mut fresh_state, "/team send shared second-action-body")
        .await;
    fresh.settle(&mut fresh_state, 1).await;
    let actions = folded_actions(&fresh.journal).await;
    assert_eq!(actions.len(), 2);
    // Membership is each dispatch row's OWN `{action, index}`: the first action
    // keeps its dispatch, and the second — refused before dispatch — owns none.
    // **M08 (join by peer or by "newest dispatch") → RED** here: either defect
    // hands the first action's task to the second (or steals it from the first).
    assert_eq!(
        actions[0].recipients[0].tasks.len(),
        1,
        "the first action owns its own dispatch: {:?}",
        actions[0].recipients[0].tasks
    );
    assert!(
        actions[1].recipients[0].tasks.is_empty(),
        "the refused action owns no dispatch: {:?}",
        actions[1].recipients[0].tasks
    );
    assert!(matches!(
        actions[0].recipients[0].settlement,
        Some(rustain::domain::models::SendSettlement::Delivered { .. })
    ));
    assert!(matches!(
        actions[1].recipients[0].settlement,
        Some(rustain::domain::models::SendSettlement::Unreachable { .. })
    ));
    let rows = read_room_events(&fresh.journal).await;
    let action_ids: Vec<_> = rows
        .iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::TeamSendActionRecorded {
                action, recipients, ..
            } => {
                assert_eq!(recipients.len(), 1);
                assert_eq!(recipients[0].alias, "shared");
                Some(action.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(action_ids.len(), 2);
    assert_eq!(action_ids[0], first_action);
    assert_ne!(action_ids[0], action_ids[1]);
    let outcomes: Vec<_> = rows
        .iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::TeamSendRecipientSettled {
                action, settlement, ..
            } => Some((action.clone(), settlement.clone())),
            _ => None,
        })
        .collect();
    assert!(outcomes.iter().any(|(id, outcome)| id == &action_ids[0]
        && matches!(
            outcome,
            rustain::domain::models::SendSettlement::Delivered { .. }
        )));
    assert!(outcomes.iter().any(|(id, outcome)| id == &action_ids[1]
        && matches!(
            outcome,
            rustain::domain::models::SendSettlement::Unreachable { .. }
        )));
    host.stop().await;
}

/// **K01 id-shape leg. Mutant M01 (mint a repeated or overlong action id) →
/// RED.** Positive control: every ID has the host prefix and the maximum
/// user-typable journal length while remaining distinct across many mints.
#[test]
fn action_ids_are_short_and_unique() {
    let ids: Vec<_> = (0..256).map(|_| SendActionId::mint()).collect();
    let unique: std::collections::HashSet<_> = ids.iter().map(|id| id.as_str()).collect();
    assert_eq!(unique.len(), ids.len());
    assert!(
        ids.iter()
            .all(|id| id.as_str().starts_with("sa_") && id.as_str().len() <= 16)
    );
}

/// **K01 compatibility leg. Mutant M05 (serialize absent `action` on a legacy
/// dispatch) → RED.** Positive control: a no-action row deserializes and
/// serializes byte-for-byte with today's legacy format.
#[test]
fn legacy_dispatch_row_serialization_is_byte_identical() {
    let peer = rustain::domain::models::a2a_peer_spec::alias_pseudonym("peer-test");
    let peer = serde_json::to_string(&peer).expect("serialize pseudonym");
    let legacy = format!(
        r#"{{"event":"remote_envelope_dispatched","peer":{peer},"task":"task-1","bytes":7}}"#
    );
    let parsed: RoomEvent = serde_json::from_str(&legacy).expect("legacy dispatch row");
    assert!(matches!(
        parsed,
        RoomEvent::RemoteEnvelopeDispatched {
            action: None,
            act: DispatchAct::Task,
            ..
        }
    ));
    assert_eq!(
        serde_json::to_string(&parsed).expect("serialize legacy row"),
        legacy
    );
}

/// **K01 forward-compatibility leg. Mutant M05 (new action tag fails an older
/// event reader) → RED.** Positive control: the prior dispatch tag is still
/// recognized by that older closed enum.
#[test]
fn older_event_reader_treats_new_action_tag_as_unrecognized() {
    #[derive(serde::Deserialize)]
    #[serde(tag = "event", rename_all = "snake_case")]
    enum OlderRoomEvent {
        RemoteEnvelopeDispatched,
        #[serde(other)]
        Unrecognized,
    }
    let newer = r#"{"event":"team_send_action_recorded","action":"sa_test","recipients":[],"message_type":"unknown","body_hash":"0000000000000000000000000000000000000000000000000000000000000000","body_bytes":0}"#;
    assert!(matches!(
        serde_json::from_str::<OlderRoomEvent>(newer).expect("new row readable"),
        OlderRoomEvent::Unrecognized
    ));
    assert!(matches!(
        serde_json::from_str::<OlderRoomEvent>(r#"{"event":"remote_envelope_dispatched"}"#)
            .expect("known row"),
        OlderRoomEvent::RemoteEnvelopeDispatched
    ));
}
/// Force two real sends to the same alias to interleave: action A's task
/// dispatch is held after action B is recorded and dispatched.
/// **M08 (join by peer / by newest dispatch) → RED** if either action inherits
/// the other's task. The exact membership assertions below must fail under
/// either mutant. Positive control: the same interleaving passes unmutated.
#[tokio::test]
async fn interleaved_dispatches_keep_their_own_action_membership() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let journal = Journal::open().await;
    let gate = Arc::new(GateRoomJournal::new(journal.room.clone(), is_task_dispatch));
    let gate_sink: Arc<dyn rustain::domain::ports::RoomJournal> = gate.clone();
    let mut sender_a =
        Sender::with_journal(vec![host.peer("shared")], gate_sink, journal.clone()).await;
    let mut state_a = TuiState::new(100, 40);
    let send_a = tokio::spawn(async move {
        sender_a
            .command(&mut state_a, "/team send shared first-body")
            .await;
        sender_a.settle(&mut state_a, 1).await;
    });
    gate.wait_started().await;

    let mut sender_b = Sender::with_journal(
        vec![host.peer("shared")],
        journal.room.clone(),
        journal.clone(),
    )
    .await;
    let mut state_b = TuiState::new(100, 40);
    sender_b
        .command(&mut state_b, "/team send shared second-body")
        .await;
    sender_b.settle(&mut state_b, 1).await;

    gate.release();
    send_a.await.expect("first sender task");

    let rows = read_room_events(&journal).await;
    let action_ids: Vec<_> = rows
        .iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::TeamSendActionRecorded { action, .. } => Some(action.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(action_ids.len(), 2);
    let dispatch_tasks = |action: &SendActionId| {
        rows.iter()
            .filter_map(|(_, event)| match event {
                RoomEvent::RemoteEnvelopeDispatched {
                    task: Some(task),
                    action: Some(reference),
                    ..
                } if reference.action == *action && reference.index == 0 => Some(task.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let task_a = dispatch_tasks(&action_ids[0]);
    let task_b = dispatch_tasks(&action_ids[1]);
    assert_eq!(
        task_a.len(),
        1,
        "action A has one explicitly correlated dispatch"
    );
    assert_eq!(
        task_b.len(),
        1,
        "action B has one explicitly correlated dispatch"
    );

    let actions = folded_actions(&journal).await;
    assert_eq!(actions.len(), 2);
    assert_eq!(
        actions[0].recipients[0].tasks, task_a,
        "action A owns EXACTLY its own task id"
    );
    assert_eq!(
        actions[1].recipients[0].tasks, task_b,
        "action B owns EXACTLY its own task id"
    );
    for action in &actions {
        assert!(matches!(
            action.recipients[0].settlement,
            Some(rustain::domain::models::SendSettlement::Delivered { .. })
        ));
    }
    host.stop().await;
}

/// A recorded `NotSent` cannot publish a terminal row when both its dispatch
/// and typed-settlement appends fail. An append-enabled send remains visible
/// and durable, while the action-record-failed path stays covered separately.
#[tokio::test]
async fn recorded_not_sent_waits_for_durable_settlement_after_both_append_failures() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let journal = Journal::open().await;
    let settlement_failure = Arc::new(RecordingRoomJournal::new(
        journal.room.clone(),
        Some(RejectAppend::new(is_recipient_settlement, 0)),
    ));
    let dispatch_failure = Arc::new(RecordingRoomJournal::new(
        settlement_failure.clone(),
        Some(RejectAppend::new(is_task_dispatch, 0)),
    ));
    let sink: Arc<dyn rustain::domain::ports::RoomJournal> = dispatch_failure.clone();
    let sender = Sender::with_journal(vec![host.peer("failed")], sink, journal).await;
    let visible = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let visible_on_settle = visible.clone();
    let failed_recipients = vec!["failed".to_owned()];
    coordinate_send(
        sender.runtime(),
        SendRequest {
            recipients: &failed_recipients,
            text: "combined append failure",
            message_type: None,
            reply_to: None,
            subject: None,
            stories: None,
            authorship: rustain::domain::models::Authorship::HumanWritten,
            send_authorization: rustain::domain::models::SendAuthorization::HumanDecided,
        },
        CancellationToken::new(),
        |_| {},
        move |_, _| {
            visible_on_settle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        },
    )
    .await
    .expect("the recorded send action itself succeeds");

    assert!(
        dispatch_failure
            .observed
            .lock()
            .iter()
            .any(is_task_dispatch),
        "the dispatch append was attempted and refused"
    );
    assert!(
        settlement_failure
            .observed
            .lock()
            .iter()
            .any(is_recipient_settlement),
        "the typed settlement append was also attempted and refused"
    );
    assert_eq!(
        visible.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a recorded NotSent recipient is not visible without its durable settlement"
    );
    let recorded = read_room_events(&sender.journal).await;
    assert!(
        recorded
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
    );
    assert!(
        !recorded.iter().any(|(_, event)| matches!(
            event,
            RoomEvent::RemoteEnvelopeDispatched { .. } | RoomEvent::TeamSendRecipientSettled { .. }
        )),
        "neither failed append may appear in the durable journal"
    );
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemReceived { .. })),
        "a failed dispatch append prevents the POST"
    );

    let healthy = Sender::start(vec![host.peer("healthy")]).await;
    let control_visible = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let control_on_settle = control_visible.clone();
    let control_recipients = vec!["healthy".to_owned()];
    coordinate_send(
        healthy.runtime(),
        SendRequest {
            recipients: &control_recipients,
            text: "append-enabled control",
            message_type: None,
            reply_to: None,
            subject: None,
            stories: None,
            authorship: rustain::domain::models::Authorship::HumanWritten,
            send_authorization: rustain::domain::models::SendAuthorization::HumanDecided,
        },
        CancellationToken::new(),
        |_| {},
        move |_, _| {
            control_on_settle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        },
    )
    .await
    .expect("the append-enabled control send");
    assert_eq!(
        control_visible.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a durable settlement still reaches the visible callback"
    );
    assert!(
        read_room_events(&healthy.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::TeamSendRecipientSettled { .. })),
        "the visible control has its durable typed settlement"
    );
    assert!(
        read_room_events(&host.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { .. })),
        "the append-enabled control reaches the same recipient"
    );
    host.stop().await;
}
