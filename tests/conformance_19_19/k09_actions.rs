use super::*;

const ACTION_SET: Duration = Duration::from_secs(45);

async fn send_architecture_update(
    sender: &mut Sender,
    aliases: &str,
    body: &str,
) -> (TuiState, String) {
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            &format!("/team send --type architecture_update {aliases} {body}"),
        )
        .await;
    let block_id = format!("team-send-{}", state.team_send_next_id);
    for _ in aliases.split(',') {
        settle_send(sender, &mut state).await;
    }
    (state, block_id)
}

async fn architecture_item_id(host: &Host, body: &str) -> String {
    let rows = room_events(&host.journal).await;
    let received = rows.iter().find_map(|(_, event)| match event {
        RoomEvent::RecipientItemReceived {
            address, content, ..
        } if content == body => Some((address.item().as_str().to_owned(), event)),
        _ => None,
    });
    let (item_id, event) = received.expect("actual architecture_update HTTP ingress");
    assert_eq!(
        serde_json::to_value(event).expect("serialize received event")["message_type"],
        "architecture_update",
        "K09: ingress persists the semantic type rather than inferring it from body text"
    );
    item_id
}

async fn stop_host(host: Host) {
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

async fn send_ack(
    stream: &mut tokio::net::UnixStream,
    item_id: &str,
) -> (
    RecipientAction,
    RecipientActionTarget,
    RecipientActionOutcome,
) {
    write_frame(
        stream,
        &ClientFrame::AcknowledgeRecipientItem {
            item_id: item_id.to_owned(),
        },
    )
    .await
    .expect("write actual acknowledgement frame");
    recipient_action_result(stream, None).await
}

#[tokio::test]
async fn k09_architecture_update_ack_is_durable_idempotent_and_discussion_remains_possible() {
    // The host fixture's production EffectiveDeliveryPolicy defaults to auto;
    // its explicit override affects only consultation, not architecture_update.
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let socket = SocketService::start(&mut host).await;
    let (mut writer, mode, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    assert_eq!(mode, AttachMode::ReadWrite);

    let mut sender = new_sender(vec![host.peer("adr-ack")]).await;
    let body = "ADR-047: adopt the stable storage boundary";
    let (_, _) = send_architecture_update(&mut sender, "adr-ack", body).await;
    let item_id = architecture_item_id(&host, body).await;
    let rows_before_ack = room_events(&host.journal).await;
    assert!(rows_before_ack.iter().all(|(_, event)| !matches!(
        event,
        RoomEvent::RecipientItemAcknowledged { .. } | RoomEvent::RecipientItemReplied { .. }
    )));
    let before_projection = rustain::adapters::policy::JournalRecipientItemProjection::from_entries(
        &RoomJournalReader::load_entries(host.journal.node.as_ref())
            .await
            .expect("read received item"),
    );
    assert!(
        matches!(
            before_projection
                .find_by_id(&item_id)
                .expect("received item")
                .state,
            rustain::domain::models::RecipientItemState::Received { .. }
        ),
        "K09: arrival and auto-response do not acknowledge the item"
    );
    let durable = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("read auto response")
        .expect("persisted conversation");
    assert!(
        durable.peer_row_controls.values().any(|control| {
            control
                .item_address
                .as_ref()
                .is_some_and(|address| address.item().as_str() == item_id.as_str())
                && control.stage == PeerRowStage::Settled
                && !control.node_resolvable
        }),
        "K09: default auto policy has a settled production response row for this item"
    );

    let requests_before = host.requests.lock().len();
    let nodes_before = host.node_tree.list().await;
    let (action, target, outcome) = send_ack(&mut writer, &item_id).await;
    assert_eq!(action, RecipientAction::Acknowledge);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    assert_eq!(outcome, RecipientActionOutcome::Applied { reply: None });

    let after_first = room_events(&host.journal).await;
    assert_eq!(
        after_first
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { address, .. } if address.item().as_str() == item_id.as_str()))
            .count(),
        1,
        "K09: Applied follows exactly one durable acknowledgement"
    );
    assert!(
        after_first.iter().all(|(_, event)| !matches!(
            event,
            RoomEvent::RecipientItemReplied { answers, .. } if answers.as_str() == item_id.as_str()
        )),
        "K09: acknowledgement does not manufacture a reply turn"
    );
    assert_eq!(
        host.node_tree.list().await,
        nodes_before,
        "K09: Ack does not resolve or mutate a node"
    );
    assert_eq!(
        host.requests.lock().len(),
        requests_before,
        "K09: Ack starts no provider/model turn"
    );

    let (action, target, outcome) = send_ack(&mut writer, &item_id).await;
    assert_eq!(action, RecipientAction::Acknowledge);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    assert_eq!(
        outcome,
        RecipientActionOutcome::Idempotent,
        "K09: repeated acknowledgement reports the already-recorded no-op"
    );
    assert_eq!(
        room_events(&host.journal)
            .await
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { address, .. } if address.item().as_str() == item_id.as_str()))
            .count(),
        1,
        "K09: repeated Ack does not append a duplicate event"
    );
    assert_eq!(host.node_tree.list().await, nodes_before);
    assert_eq!(host.requests.lock().len(), requests_before);

    let answer = "Human decision: keep the storage boundary and revise the migration note.";
    write_frame(
        &mut writer,
        &ClientFrame::ReplyToRecipientItem {
            item_id: item_id.clone(),
            text: answer.to_owned(),
            verdict: None,
        },
    )
    .await
    .expect("write actual discussion frame");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Reply);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    let receipt = match outcome {
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } => receipt,
        other => panic!("K09: acknowledged item remains discussable: {other:?}"),
    };
    assert_eq!(receipt.item_id, item_id);
    assert_eq!(receipt.kind, rustain::domain::models::ReplyKind::Answer);
    let after_discussion = room_events(&host.journal).await;
    assert!(
        after_discussion.iter().any(|(_, event)| matches!(
            event,
            RoomEvent::RecipientItemReplied {
                address,
                answers,
                content,
                kind: rustain::domain::models::ReplyKind::Answer,
                authorship: rustain::domain::models::Authorship::HumanWritten,
                via_node: None,
                ..
            } if answers.as_str() == item_id.as_str()
                && address.turn().as_str() == receipt.turn_id.as_str()
                && content == answer
        )),
        "K09: Discuss stores the exact human answer as its own item reply turn"
    );
    assert_eq!(host.node_tree.list().await, nodes_before);
    assert_eq!(host.requests.lock().len(), requests_before);

    drop(writer);
    drop(socket);
    stop_host(host).await;
}

#[tokio::test]
async fn k09_architecture_update_fanout_keeps_allowed_and_refused_recipients_independent() {
    let mut allowed = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let denied = Host::serve(A2aAdmissionPolicy::Deny, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![allowed.peer("allowed"), denied.peer("denied")]).await;
    let mixed_body = "ADR-047 mixed fanout control";
    let (mixed_state, mixed_block) =
        send_architecture_update(&mut sender, "allowed,denied", mixed_body).await;
    let mixed_outcomes = &mixed_state.feedback_blocks[&mixed_block].message;
    let mixed_rows: Vec<_> = mixed_outcomes.lines().collect();
    assert!(
        mixed_rows
            .iter()
            .any(|row| row.contains("allowed") && row.contains("delivered"))
            && mixed_rows
                .iter()
                .any(|row| row.contains("denied") && row.contains("declined")),
        "K09: each recipient row has its own outcome: {mixed_outcomes}"
    );
    assert_eq!(mixed_rows.len(), 2);
    let allowed_item = architecture_item_id(&allowed, mixed_body).await;
    assert!(received(&room_events(&denied.journal).await).is_empty());

    let socket = SocketService::start(&mut allowed).await;
    let (mut allowed_writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    let (action, target, outcome) = send_ack(&mut allowed_writer, &allowed_item).await;
    assert_eq!(action, RecipientAction::Acknowledge);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: allowed_item.clone()
        }
    );
    assert_eq!(outcome, RecipientActionOutcome::Applied { reply: None });
    assert_eq!(
        room_events(&denied.journal)
            .await
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { .. }))
            .count(),
        0,
        "K09: a refused recipient has no item to acknowledge and no local Ack effect"
    );

    let mut second_allowed =
        Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut both_sender =
        new_sender(vec![allowed.peer("allowed"), second_allowed.peer("second")]).await;
    let both_body = "ADR-047 both-allowed control";
    let (both_state, both_block) =
        send_architecture_update(&mut both_sender, "allowed,second", both_body).await;
    let both_outcomes = &both_state.feedback_blocks[&both_block].message;
    let both_rows: Vec<_> = both_outcomes.lines().collect();
    assert_eq!(both_rows.len(), 2, "{both_outcomes}");
    assert_eq!(
        both_rows
            .iter()
            .filter(|row| row.contains("delivered"))
            .count(),
        2,
        "K09: each allowed recipient row is delivered: {both_outcomes}"
    );
    assert!(
        both_rows
            .iter()
            .any(|row| row.contains("allowed") && row.contains("delivered"))
    );
    assert!(
        both_rows
            .iter()
            .any(|row| row.contains("second") && row.contains("delivered"))
    );
    let allowed_second_item = architecture_item_id(&allowed, both_body).await;
    let second_item = architecture_item_id(&second_allowed, both_body).await;

    let second_socket = SocketService::start(&mut second_allowed).await;
    let (mut second_writer, _, _) = second_socket
        .attach(ConnectionTier::TrustedLocal, false)
        .await;
    let (first_result, second_result) = tokio::join!(
        send_ack(&mut allowed_writer, &allowed_second_item),
        send_ack(&mut second_writer, &second_item),
    );
    assert_eq!(
        first_result,
        (
            RecipientAction::Acknowledge,
            RecipientActionTarget::Item {
                item_id: allowed_second_item.clone()
            },
            RecipientActionOutcome::Applied { reply: None },
        )
    );
    assert_eq!(
        second_result,
        (
            RecipientAction::Acknowledge,
            RecipientActionTarget::Item {
                item_id: second_item.clone()
            },
            RecipientActionOutcome::Applied { reply: None },
        )
    );
    for (journal, item_id) in [
        (&allowed.journal, allowed_second_item),
        (&second_allowed.journal, second_item),
    ] {
        assert_eq!(
            room_events(journal)
                .await
                .iter()
                .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { address, .. } if address.item().as_str() == item_id.as_str()))
                .count(),
            1,
            "K09: each allowed recipient durably acknowledges only its own host-minted item"
        );
    }

    drop(allowed_writer);
    drop(second_writer);
    drop(socket);
    drop(second_socket);
    stop_host(allowed).await;
    stop_host(denied).await;
    stop_host(second_allowed).await;
}

#[tokio::test]
async fn k09_ack_refusals_are_correlated_and_have_no_acknowledgement_effect() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![host.peer("ack-refusals")]).await;
    let body = "ADR-047 acknowledgement refusal controls";
    let (_, _) = send_architecture_update(&mut sender, "ack-refusals", body).await;
    let item_id = architecture_item_id(&host, body).await;
    let socket = SocketService::start(&mut host).await;

    let (mut read_only, mode, _) = socket.attach(ConnectionTier::TrustedLocal, true).await;
    assert_eq!(mode, AttachMode::ReadOnly);
    let result = send_ack(&mut read_only, &item_id).await;
    assert_eq!(result.0, RecipientAction::Acknowledge);
    assert_eq!(
        result.1,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    assert_eq!(
        result.2,
        RecipientActionOutcome::Refused {
            error: ProtocolError::ReadOnly
        }
    );

    let (mut peer, _, _) = socket.attach(ConnectionTier::Peer, false).await;
    let result = send_ack(&mut peer, &item_id).await;
    assert_eq!(result.0, RecipientAction::Acknowledge);
    assert_eq!(
        result.1,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    assert!(matches!(
        result.2,
        RecipientActionOutcome::Refused {
            error: ProtocolError::PeerVerification(message),
        } if message.contains("trusted-local only")
    ));

    let (mut writer, mode, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    assert_eq!(mode, AttachMode::ReadWrite);
    let missing_id = "item-id-never-minted".to_owned();
    let result = send_ack(&mut writer, &missing_id).await;
    assert_eq!(result.0, RecipientAction::Acknowledge);
    assert_eq!(
        result.1,
        RecipientActionTarget::Item {
            item_id: missing_id
        }
    );
    assert_eq!(
        result.2,
        RecipientActionOutcome::Refused {
            error: ProtocolError::Malformed("recipient item not found".to_owned()),
        }
    );

    write_frame(
        &mut writer,
        &ClientFrame::RemoveRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("remove actual recipient item");
    let result = send_ack(&mut writer, &item_id).await;
    assert_eq!(result.0, RecipientAction::Acknowledge);
    assert_eq!(
        result.1,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    assert_eq!(
        result.2,
        RecipientActionOutcome::Refused {
            error: ProtocolError::Malformed("recipient item already removed".to_owned()),
        }
    );
    assert_eq!(
        room_events(&host.journal)
            .await
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { .. }))
            .count(),
        0,
        "K09: read-only, peer-tier, missing and removed refusals append no acknowledgement"
    );

    drop(read_only);
    drop(peer);
    drop(writer);
    drop(socket);
    stop_host(host).await;
}

struct FailAckAppend {
    inner: Arc<dyn RoomJournal>,
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl RoomJournal for FailAckAppend {
    async fn record_event(
        &self,
        event: RoomEvent,
    ) -> Result<(), rustain::domain::ports::RoomJournalError> {
        if matches!(event, RoomEvent::RecipientItemAcknowledged { .. }) {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Err(rustain::domain::ports::RoomJournalError::Append(
                "injected acknowledgement append failure".to_owned(),
            ));
        }
        self.inner.record_event(event).await
    }
}

#[tokio::test]
async fn k09_failed_ack_append_returns_correlated_refusal_without_durable_ack() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    let wrapped_attempts = attempts.clone();
    options.attach_wrap = Some(Arc::new(move |inner| {
        Arc::new(FailAckAppend {
            inner,
            attempts: wrapped_attempts.clone(),
        })
    }));
    let mut host = Host::serve_with(options).await;
    let mut sender = new_sender(vec![host.peer("failed-ack")]).await;
    let body = "ADR-047 failed acknowledgement append";
    let (_, _) = send_architecture_update(&mut sender, "failed-ack", body).await;
    let item_id = architecture_item_id(&host, body).await;
    let socket = SocketService::start(&mut host).await;
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;

    let result = send_ack(&mut writer, &item_id).await;
    assert_eq!(result.0, RecipientAction::Acknowledge);
    assert_eq!(result.1, RecipientActionTarget::Item { item_id });
    assert_eq!(
        result.2,
        RecipientActionOutcome::Refused {
            error: ProtocolError::Internal(
                "room journal append failed: injected acknowledgement append failure".to_owned(),
            ),
        }
    );
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        room_events(&host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemAcknowledged { .. })),
        "K09: failed append is not presented as durable and leaves no Ack event"
    );

    drop(writer);
    drop(socket);
    stop_host(host).await;
}

#[derive(Default)]
struct AckAppendGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct GateAckAppend {
    inner: Arc<dyn RoomJournal>,
    gate: Arc<AckAppendGate>,
}

#[async_trait::async_trait]
impl RoomJournal for GateAckAppend {
    async fn record_event(
        &self,
        event: RoomEvent,
    ) -> Result<(), rustain::domain::ports::RoomJournalError> {
        if matches!(event, RoomEvent::RecipientItemAcknowledged { .. }) {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        self.inner.record_event(event).await
    }
}

#[tokio::test]
async fn k09_gated_ack_and_discuss_actions_keep_distinct_targets_and_outcomes() {
    let gate = Arc::new(AckAppendGate::default());
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    let wrapped_gate = gate.clone();
    options.attach_wrap = Some(Arc::new(move |inner| {
        Arc::new(GateAckAppend {
            inner,
            gate: wrapped_gate.clone(),
        })
    }));
    let mut host = Host::serve_with(options).await;
    let socket = SocketService::start(&mut host).await;
    let (mut ack_writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    let (mut reply_writer, reply_mode, _) = socket.attach(ConnectionTier::TrustedLocal, true).await;
    assert_eq!(reply_mode, AttachMode::ReadOnly);
    let mut sender = new_sender(vec![host.peer("gated-actions")]).await;
    let first_body = "ADR-047 gated acknowledgement target";
    let second_body = "ADR-047 gated discussion target";
    let (_, _) = send_architecture_update(&mut sender, "gated-actions", first_body).await;
    let first_item = architecture_item_id(&host, first_body).await;
    let (_, _) = send_architecture_update(&mut sender, "gated-actions", second_body).await;
    let second_item = architecture_item_id(&host, second_body).await;
    assert_ne!(first_item, second_item);

    let requests_before = host.requests.lock().len();
    let nodes_before = host.node_tree.list().await;
    write_frame(
        &mut ack_writer,
        &ClientFrame::AcknowledgeRecipientItem {
            item_id: first_item.clone(),
        },
    )
    .await
    .expect("start first real socket action");
    tokio::time::timeout(ACTION_SET, gate.entered.notified())
        .await
        .expect("ack append reached the controlled journal gate");
    // The gated Ack holds this connection's reader, so no in-band fence can
    // pass it. Observe the socket for a bounded window while the append is
    // still blocked: any Applied result for the Ack here is premature.
    // A receive deadline must not cancel a partially decoded socket frame.
    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::unbounded_channel();
    let frame_reader = tokio::spawn(async move {
        while let Some(frame) = read_frame::<_, DaemonFrame>(&mut ack_writer)
            .await
            .expect("valid acknowledgement daemon frame")
        {
            if frames_tx.send(frame).is_err() {
                break;
            }
        }
    });
    let observe_until = tokio::time::Instant::now() + std::time::Duration::from_millis(300);
    while let Ok(frame) = tokio::time::timeout_at(observe_until, frames_rx.recv()).await {
        if let DaemonFrame::RecipientActionResult {
            action, outcome, ..
        } = frame.expect("open socket")
        {
            assert!(
                !matches!(action, RecipientAction::Acknowledge)
                    || !matches!(outcome, RecipientActionOutcome::Applied { .. }),
                "K09: correlated Ack cannot be Applied before durable append"
            );
        }
    }
    let ack_result = Box::pin(async {
        loop {
            match tokio::time::timeout(ACTION_SET, frames_rx.recv())
                .await
                .expect("acknowledgement result deadline")
                .expect("open acknowledgement socket")
            {
                DaemonFrame::RecipientActionResult {
                    action,
                    target,
                    outcome,
                } => {
                    break (action, target, outcome);
                }
                DaemonFrame::Error(error) => panic!("action result was not correlated: {error:?}"),
                _ => {}
            }
        }
    });

    // The first outcome is held behind a real durable append. Send a different
    // action and item target through another authenticated socket before opening
    // that boundary; the server must return each request to its own connection.
    write_frame(
        &mut reply_writer,
        &ClientFrame::ReplyToRecipientItem {
            item_id: second_item.clone(),
            text: "A separate human discussion while Ack is pending.".to_owned(),
            verdict: None,
        },
    )
    .await
    .expect("send second action while first append is gated");
    tokio::task::yield_now().await;
    gate.release.notify_one();

    let (ack_result, reply_result) =
        tokio::join!(ack_result, recipient_action_result(&mut reply_writer, None),);
    assert_eq!(ack_result.0, RecipientAction::Acknowledge);
    assert_eq!(
        ack_result.1,
        RecipientActionTarget::Item {
            item_id: first_item.clone()
        }
    );
    assert_eq!(
        ack_result.2,
        RecipientActionOutcome::Applied { reply: None }
    );
    assert_eq!(reply_result.0, RecipientAction::Reply);
    assert_eq!(
        reply_result.1,
        RecipientActionTarget::Item {
            item_id: second_item.clone()
        }
    );
    assert_eq!(
        reply_result.2,
        RecipientActionOutcome::Refused {
            error: ProtocolError::ReadOnly,
        },
        "K09: the different pending action is refused only by its own read-only gate"
    );

    let events = room_events(&host.journal).await;
    assert_eq!(
        events
            .iter()
            .filter(|(_, event)| matches!(
                event,
                RoomEvent::RecipientItemAcknowledged { address, .. }
                    if address.item().as_str() == first_item.as_str()
            ))
            .count(),
        1,
        "gated Ack durably records exactly one receipt",
    );
    assert!(
        events.iter().all(|(_, event)| !matches!(
            event,
            RoomEvent::RecipientItemReplied { answers, .. }
                if answers.as_str() == second_item.as_str()
        )),
        "K09: the correlated read-only refusal creates no discussion turn"
    );
    assert_eq!(
        events
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { address, .. } if address.item().as_str() == first_item.as_str()))
            .count(),
        1
    );
    assert_eq!(host.node_tree.list().await, nodes_before);
    assert_eq!(host.requests.lock().len(), requests_before);

    frame_reader.abort();
    drop(reply_writer);
    drop(socket);
    stop_host(host).await;
}
