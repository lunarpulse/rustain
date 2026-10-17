use super::*;
async fn inspect_context(stream: &mut tokio::net::UnixStream) -> (Vec<String>, String) {
    write_frame(stream, &ClientFrame::InspectRecipientContext)
        .await
        .expect("inspect context through actual socket");
    loop {
        if let DaemonFrame::RecipientContextState {
            selected_item_ids,
            inventory,
            show_inventory: true,
        } = socket_frame(stream).await
        {
            return (selected_item_ids, inventory);
        }
    }
}

async fn send_consultation(host: &Host, text: &str) -> String {
    let mut sender = new_sender(vec![host.peer("k11-authority")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            &format!("/team send --type consultation k11-authority {text}"),
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    room_events(&host.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address, content, ..
            } if content == text => Some(address.item().as_str().to_owned()),
            _ => None,
        })
        .expect("actual signed HTTP arrival persisted as a recipient item")
}

async fn assert_action(
    stream: &mut tokio::net::UnixStream,
    action: RecipientAction,
    item_id: &str,
    outcome: RecipientActionOutcome,
) {
    let (actual_action, actual_target, actual_outcome) =
        recipient_action_result(stream, None).await;
    assert_eq!(
        actual_action, action,
        "correlated recipient action result preserves its originating action identity"
    );
    assert_eq!(
        actual_target,
        RecipientActionTarget::Item {
            item_id: item_id.to_owned()
        }
    );
    assert_eq!(actual_outcome, outcome);
}

async fn assert_context_selection_and_result(stream: &mut tokio::net::UnixStream, item_id: &str) {
    let mut saw_async_state = false;
    loop {
        match socket_frame(stream).await {
            DaemonFrame::PeerRowOperation(_) => {}
            DaemonFrame::Event(_) => {}
            DaemonFrame::RecipientContextState {
                selected_item_ids,
                show_inventory,
                ..
            } => {
                assert!(!show_inventory);
                assert_eq!(selected_item_ids, vec![item_id.to_owned()]);
                saw_async_state = true;
            }
            DaemonFrame::RecipientActionResult {
                action,
                target,
                outcome,
            } => {
                assert!(
                    saw_async_state,
                    "async state precedes correlated action outcome"
                );
                assert_eq!(action, RecipientAction::ContextSelect);
                assert_eq!(
                    target,
                    RecipientActionTarget::Item {
                        item_id: item_id.to_owned()
                    }
                );
                assert_eq!(
                    outcome,
                    RecipientActionOutcome::ContextSelection {
                        selected: true,
                        changed: true,
                    }
                );
                return;
            }
            DaemonFrame::Error(error) => {
                panic!("context action result was not correlated: {error:?}")
            }
            other => panic!("unexpected context-action frame: {other:?}"),
        }
    }
}

#[tokio::test]
async fn k11_read_only_authority_refuses_defer_and_context_on_off_without_side_effects() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let socket = SocketService::start(&mut host).await;
    let (_writer, writer_mode, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    assert_eq!(writer_mode, AttachMode::ReadWrite);
    let item_id = send_consultation(&host, "k11-read-only-action-target").await;
    assert!(
        host.requests.lock().is_empty(),
        "typed wait arrival does not call the model"
    );

    let (mut reader, mode, _) = socket.attach(ConnectionTier::TrustedLocal, true).await;
    assert_eq!(mode, AttachMode::ReadOnly);
    let events_before = room_events(&host.journal).await;
    let requests_before = host.requests.lock().len();

    write_frame(
        &mut reader,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("send defer through actual read-only socket");
    assert_action(
        &mut reader,
        RecipientAction::Defer,
        &item_id,
        RecipientActionOutcome::Refused {
            error: ProtocolError::ReadOnly,
        },
    )
    .await;

    for selected in [true, false] {
        write_frame(
            &mut reader,
            &ClientFrame::SetRecipientContext {
                item_id: item_id.clone(),
                selected,
            },
        )
        .await
        .expect("send context on/off through actual read-only socket");
        assert_action(
            &mut reader,
            if selected {
                RecipientAction::ContextSelect
            } else {
                RecipientAction::ContextRemove
            },
            &item_id,
            RecipientActionOutcome::Refused {
                error: ProtocolError::ReadOnly,
            },
        )
        .await;
    }

    let (selected, _) = inspect_context(&mut reader).await;
    assert!(selected.is_empty());
    assert_eq!(room_events(&host.journal).await, events_before);
    assert_eq!(host.requests.lock().len(), requests_before);
}

#[tokio::test]
async fn k11_peer_attach_withholds_local_controls_and_rejects_context_inventory() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let socket = SocketService::start(&mut host).await;
    let (mut writer, mode, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    assert_eq!(mode, AttachMode::ReadWrite);
    let item_id = send_consultation(&host, "k11-peer-must-not-see-selection").await;
    let (_, task, _) = received(&room_events(&host.journal).await)
        .into_iter()
        .find(|(id, _, _)| id == &item_id)
        .expect("actual incoming consultation");
    let node = waiting_node(&host, &task).await;
    let (operations, _) = history_fence(&mut writer).await;
    let row_id = format!("peer-response-{node}");
    assert!(operations.iter().any(|operation| matches!(
        operation,
        PeerRowOperation::Upsert { message, .. } if message.id == row_id
    )));
    write_frame(
        &mut writer,
        &ClientFrame::SetRecipientContext {
            item_id: item_id.clone(),
            selected: true,
        },
    )
    .await
    .expect("select by actual received item id");
    assert_action(
        &mut writer,
        RecipientAction::ContextSelect,
        &item_id,
        RecipientActionOutcome::ContextSelection {
            selected: true,
            changed: true,
        },
    )
    .await;
    let (trusted_ids, _) = inspect_context(&mut writer).await;
    assert_eq!(trusted_ids, vec![item_id.clone()]);
    let (_, _, trusted_snapshot) = socket.attach(ConnectionTier::TrustedLocal, true).await;
    assert!(
        trusted_snapshot.peer_row_controls.contains_key(&row_id),
        "trusted-local observer has the actual peer-row control sidecar"
    );

    let (mut peer, mode, snapshot) = socket.attach(ConnectionTier::Peer, false).await;
    assert_eq!(mode, AttachMode::ReadOnly);
    assert!(snapshot.recipient_context_ids.is_empty());
    assert!(snapshot.peer_row_controls.is_empty());
    assert_eq!(snapshot.peer_row_revision, 0);

    let events_before_inspection = room_events(&host.journal).await;
    let requests_before_inspection = host.requests.lock().len();
    write_frame(&mut peer, &ClientFrame::InspectRecipientContext)
        .await
        .expect("request context inventory through actual peer socket");
    match socket_frame(&mut peer).await {
        DaemonFrame::Error(ProtocolError::PeerVerification(_)) => {}
        other => panic!("peer context inspection must be denied without inventory: {other:?}"),
    }
    for (frame, action) in [
        (
            ClientFrame::DeferRecipientItem {
                item_id: item_id.clone(),
            },
            RecipientAction::Defer,
        ),
        (
            ClientFrame::SetRecipientContext {
                item_id: item_id.clone(),
                selected: true,
            },
            RecipientAction::ContextSelect,
        ),
        (
            ClientFrame::SetRecipientContext {
                item_id: item_id.clone(),
                selected: false,
            },
            RecipientAction::ContextRemove,
        ),
    ] {
        write_frame(&mut peer, &frame).await.unwrap();
        let (actual_action, target, outcome) = recipient_action_result(&mut peer, None).await;
        assert_eq!(actual_action, action);
        assert_eq!(
            target,
            RecipientActionTarget::Item {
                item_id: item_id.clone()
            }
        );
        assert!(matches!(
            outcome,
            RecipientActionOutcome::Refused {
                error: ProtocolError::PeerVerification(_)
            }
        ));
    }
    assert_eq!(inspect_context(&mut writer).await.0, vec![item_id]);
    assert_eq!(room_events(&host.journal).await, events_before_inspection);
    assert_eq!(host.requests.lock().len(), requests_before_inspection);
}

#[tokio::test]
async fn k11_live_context_broadcast_excludes_peer_tier() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let socket = SocketService::start(&mut host).await;
    let (mut writer, writer_mode, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    assert_eq!(writer_mode, AttachMode::ReadWrite);
    let (mut reader, reader_mode, _) = socket.attach(ConnectionTier::TrustedLocal, true).await;
    assert_eq!(reader_mode, AttachMode::ReadOnly);

    let text = "k11-live-context-broadcast-tier-fence";
    let item_id = send_consultation(&host, text).await;
    let (mut peer, peer_mode, _) = socket.attach(ConnectionTier::Peer, false).await;
    assert_eq!(peer_mode, AttachMode::ReadOnly);

    write_frame(
        &mut writer,
        &ClientFrame::SetRecipientContext {
            item_id: item_id.clone(),
            selected: true,
        },
    )
    .await
    .expect("select received item through trusted-local writer");
    assert_context_selection_and_result(&mut writer, &item_id).await;

    loop {
        match socket_frame(&mut reader).await {
            DaemonFrame::RecipientContextState {
                selected_item_ids,
                show_inventory,
                ..
            } => {
                assert!(!show_inventory);
                assert_eq!(selected_item_ids, vec![item_id.clone()]);
                break;
            }
            DaemonFrame::Event(_) | DaemonFrame::PeerRowOperation(_) => {}
            DaemonFrame::RecipientActionResult { .. } => {
                panic!("trusted-local reader must not receive writer action result")
            }
            other => panic!("unexpected trusted-local context frame: {other:?}"),
        }
    }
    // The peer is read-only, so a bounded drain checks the live stream without
    // sending an unauthorized HistoryRequest.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(300);
    loop {
        match tokio::time::timeout_at(deadline, read_frame::<_, DaemonFrame>(&mut peer)).await {
            Err(_) => break,
            Ok(Ok(Some(DaemonFrame::Event(_)))) => {}
            Ok(Ok(Some(DaemonFrame::PeerRowOperation(_)))) => {}
            Ok(Ok(Some(other))) => {
                panic!("peer tier received forbidden or unexpected live frame: {other:?}")
            }
            Ok(Ok(None)) => panic!("peer connection closed during live broadcast check"),
            Ok(Err(error)) => panic!("peer frame decode failed: {error}"),
        }
    }
}
#[tokio::test]
async fn k11_http_arrival_reaches_pre_attached_reader_then_writer_actions_are_fenced() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let socket = SocketService::start(&mut host).await;
    let (mut writer, writer_mode, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    assert_eq!(writer_mode, AttachMode::ReadWrite);
    let (mut reader, mode, _) = socket.attach(ConnectionTier::TrustedLocal, true).await;
    assert_eq!(mode, AttachMode::ReadOnly);

    let text = "k11-http-arrival-after-read-only-attach";
    let item_id = send_consultation(&host, text).await;
    assert!(
        host.requests.lock().is_empty(),
        "wait-policy ingress makes no model request"
    );

    write_frame(
        &mut writer,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("defer received item through trusted-local writer");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(
        action,
        RecipientAction::Defer,
        "defer's correlated outcome retains the Defer action identity"
    );
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        },
        "a correlated Defer result must target the arrived item, not a row update"
    );
    assert!(matches!(
        outcome,
        RecipientActionOutcome::Applied { reply: Some(_) }
    ));

    write_frame(
        &mut writer,
        &ClientFrame::SetRecipientContext {
            item_id: item_id.clone(),
            selected: true,
        },
    )
    .await
    .expect("explicitly select arrived item through writer");
    assert_context_selection_and_result(&mut writer, &item_id).await;

    // The observer receives the asynchronous row commit and context-state
    // broadcast, not the writer's request result. History fences prior row
    // operations; the correlated Defer outcome follows its durable journal turn.
    let mut observer_ops = Vec::new();
    loop {
        match socket_frame(&mut reader).await {
            DaemonFrame::Event(_) => {}
            DaemonFrame::PeerRowOperation(operation) => {
                observer_ops.push(std::sync::Arc::unwrap_or_clone(operation));
            }
            DaemonFrame::RecipientContextState {
                selected_item_ids,
                show_inventory,
                ..
            } => {
                assert!(!show_inventory);
                assert_eq!(selected_item_ids, vec![item_id.clone()]);
                break;
            }
            DaemonFrame::RecipientActionResult { .. } => {
                panic!("read-only observer must not receive the writer's correlated outcome")
            }
            other => panic!("unexpected asynchronous observer frame: {other:?}"),
        }
    }
    let (ops, history) = history_fence(&mut reader).await;
    observer_ops.extend(ops);
    assert!(observer_ops.iter().any(|operation| matches!(
        operation,
        PeerRowOperation::Upsert {
            control: Some(control),
            ..
        } if control.item_address.as_ref().is_some_and(|address| {
            address.item().as_str() == item_id.as_str()
        })
    )));
    assert!(history.iter().any(|message| message.content.contains(text)));
    let events = room_events(&host.journal).await;
    assert!(events.iter().any(|(_, event)| matches!(event,
        RoomEvent::RecipientItemReplied { answers, content, .. }
            if answers.as_str() == item_id.as_str() && content == "Busy — defer 30m"
    )));
    assert!(host.requests.lock().is_empty());
}

#[tokio::test]
async fn k11_signed_v4_attach_is_rejected_and_signed_v7_control_succeeds() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let socket = SocketService::start(&mut host).await;
    let mut stream = tokio::net::UnixStream::connect(&socket.path)
        .await
        .expect("connect actual daemon socket");
    let nonce = match socket_frame(&mut stream).await {
        DaemonFrame::AttachChallenge { nonce } => nonce,
        other => panic!("daemon sends signed attach challenge first: {other:?}"),
    };
    let keys = tempfile::tempdir().expect("isolated signer directory");
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .expect("actual signer");
    let client_version = 4;
    let tier = ConnectionTier::TrustedLocal;
    let proof = signer.attach_proof(&nonce, client_version, tier.proof_tag(), false);
    write_frame(
        &mut stream,
        &ClientFrame::Attach {
            protocol_version: client_version,
            read_only_ok: false,
            tier,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
    .expect("write authentic v4 attach proof");
    assert!(matches!(
        socket_frame(&mut stream).await,
        DaemonFrame::Error(ProtocolError::VersionMismatch {
            daemon: PROTOCOL_VERSION,
            client: 4
        })
    ));
    drop(stream);

    let (mut current, mode, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    assert_eq!(mode, AttachMode::ReadWrite);
    assert!(inspect_context(&mut current).await.0.is_empty());
    write_frame(&mut current, &ClientFrame::Detach)
        .await
        .unwrap();
}
