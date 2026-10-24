use super::*;
use rustain::domain::events::DomainEventPayload;
use rustain::domain::models::{Authorship, ChatMessage, NoticeLevel, SendAuthorization};
use rustain::infrastructure::runtime::event_bus::RawEventKind;

// A single reader owns each framing operation. Deadlines only cancel channel
// receives, never a partially consumed socket frame.
struct K08Observer {
    writer: tokio::net::unix::OwnedWriteHalf,
    incoming: tokio::sync::mpsc::UnboundedReceiver<Result<DaemonFrame, String>>,
    reader: tokio::task::JoinHandle<()>,
    frames: Vec<DaemonFrame>,
    initial: AttachSnapshot,
}

impl Drop for K08Observer {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl K08Observer {
    async fn attach(service: &SocketService, tier: ConnectionTier) -> Self {
        let (socket, mode, initial) = attach_socket(&service.path, tier, true, None).await;
        assert_eq!(mode, AttachMode::ReadOnly, "K08 observer is read-only");
        let (mut reader, writer) = socket.into_split();
        let (tx, incoming) = tokio::sync::mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            loop {
                let frame = match read_frame::<_, DaemonFrame>(&mut reader).await {
                    Ok(Some(frame)) => Ok(frame),
                    Ok(None) => Err("observer socket closed".to_owned()),
                    Err(error) => Err(format!("observer framing failed: {error:?}")),
                };
                let terminal = frame.is_err();
                if tx.send(frame).is_err() || terminal {
                    break;
                }
            }
        });
        Self {
            writer,
            incoming,
            reader,
            frames: Vec::new(),
            initial,
        }
    }

    async fn next(&mut self) -> &DaemonFrame {
        let frame = tokio::time::timeout(SETTLE, self.incoming.recv())
            .await
            .expect("K08 observer delivery deadline")
            .expect("K08 observer reader remains alive")
            .expect("K08 observer remains attached with an open socket");
        assert!(
            !matches!(frame, DaemonFrame::Error(_) | DaemonFrame::Detached),
            "K08 observer must remain attached without terminal Error: {frame:?}"
        );
        self.frames.push(frame);
        self.frames.last().unwrap()
    }

    async fn barrier(&mut self, marker: &str) {
        loop {
            let frame = self.next().await;
            let event = match frame {
                DaemonFrame::Event(event) | DaemonFrame::ToolEventWithheld { event, .. } => event,
                _ => continue,
            };
            if matches!(&event.kind, RawEventKind::SystemNotice { message, .. } if message == marker)
            {
                break;
            }
        }
        assert!(
            !self.reader.is_finished(),
            "K08 observer socket still open at domain barrier"
        );
    }

    async fn history(&mut self) -> Vec<ChatMessage> {
        write_frame(
            &mut self.writer,
            &ClientFrame::HistoryRequest {
                before_index: None,
                count: 200,
            },
        )
        .await
        .expect("K08 FIFO history fence request");
        loop {
            if let DaemonFrame::History { messages, .. } = self.next().await {
                return messages.clone();
            }
        }
    }
}

fn k08_assert_raw_receipts(observer: &K08Observer, approved: &str, automatic: &str) {
    let mut reply_count = 0;
    let mut approved_disclosures = 0;
    let mut automatic_disclosures = 0;
    for frame in &observer.frames {
        let event = match frame {
            DaemonFrame::Event(event) | DaemonFrame::ToolEventWithheld { event, .. } => event,
            _ => continue,
        };
        let RawEventKind::DomainEvent(DomainEventPayload::Room(room)) = &event.kind else {
            continue;
        };
        let raw = serde_json::to_value(room).expect("K08 serialized raw room payload");
        match room {
            RoomEvent::RecipientItemReplied {
                via_node: Some(node),
                authorship,
                ..
            } if node.as_str() == approved => {
                reply_count += 1;
                assert_eq!(
                    *authorship,
                    Authorship::AgentComposed,
                    "K08 raw approved reply preserves today's authorship"
                );
                assert_eq!(
                    raw["authorship"], "agent-composed",
                    "K08 reply authorship remains on wire"
                );
                assert!(
                    raw.get("send_authorization").is_none(),
                    "K08 raw RecipientItemReplied leaks send_authorization: {raw}"
                );
            }
            RoomEvent::PeerDisclosure { node, .. }
                if node.as_str() == approved || node.as_str() == automatic =>
            {
                if node.as_str() == approved {
                    approved_disclosures += 1;
                } else {
                    automatic_disclosures += 1;
                }
                assert!(
                    raw.get("authorship").is_none(),
                    "K08 raw PeerDisclosure leaks authorship: {raw}"
                );
                assert!(
                    raw.get("send_authorization").is_none(),
                    "K08 raw PeerDisclosure leaks send_authorization: {raw}"
                );
            }
            _ => {}
        }
    }
    assert!(
        reply_count > 0,
        "K08 non-zero raw approved RecipientItemReplied receipt precondition"
    );
    assert!(
        approved_disclosures > 0,
        "K08 non-zero raw approved PeerDisclosure receipt precondition"
    );
    assert!(
        automatic_disclosures > 0,
        "K08 non-zero raw automatic PeerDisclosure receipt precondition"
    );
}

fn k08_assert_rows(
    rows: &[ChatMessage],
    approved: &str,
    automatic: &str,
    peer: bool,
    surface: &str,
) {
    if peer {
        for row in rows {
            let raw = serde_json::to_value(row).unwrap();
            assert!(
                raw.get("sendAuthorization").is_none(),
                "K08 Peer {surface} leaks sendAuthorization: {raw}"
            );
        }
    }
    for (node, expected) in [
        (approved, SendAuthorization::HumanDecided),
        (automatic, SendAuthorization::Automatic),
    ] {
        let id = format!("peer-response-{node}");
        let row = rows
            .iter()
            .find(|row| row.id == id)
            .unwrap_or_else(|| panic!("K08 {surface} missing actual response row {id}"));
        assert_eq!(
            row.authorship,
            Authorship::AgentComposed,
            "K08 {surface} preserves response authorship"
        );
        assert_eq!(
            row.send_authorization,
            if peer {
                SendAuthorization::Unknown
            } else {
                expected
            },
            "K08 {surface} send authorization audience boundary for {id}"
        );
    }
}

#[tokio::test]
async fn k08_peer_tier_never_sees_send_authorization_and_old_protocol_is_refused() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft).await;
    let service = SocketService::start(&mut host).await;

    // Literal Task 0 baseline, deliberately NOT PROTOCOL_VERSION - 1. An
    // invalid proof plus VersionMismatch proves refusal precedes proof admission.
    let mut old = tokio::net::UnixStream::connect(&service.path)
        .await
        .unwrap();
    let nonce = match socket_frame(&mut old).await {
        DaemonFrame::AttachChallenge { nonce } => nonce,
        other => panic!("K08 old client needs challenge: {other:?}"),
    };
    let keys = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .unwrap();
    write_frame(
        &mut old,
        &ClientFrame::Attach {
            protocol_version: 8,
            read_only_ok: false,
            tier: ConnectionTier::TrustedLocal,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof: rustain::domain::models::Ed25519Sig(vec![0u8; 64]),
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(
            socket_frame(&mut old).await,
            DaemonFrame::Error(ProtocolError::VersionMismatch {
                daemon: 11,
                client: 8
            })
        ),
        "K08 literal protocol 8 must receive VersionMismatch before registration"
    );
    assert!(
        tokio::time::timeout(SETTLE, read_frame::<_, DaemonFrame>(&mut old))
            .await
            .expect("K08 old socket closure deadline")
            .expect("K08 old socket read")
            .is_none(),
        "K08 refused old client must close without registration"
    );

    let (mut writer, mode, _) = service.attach(ConnectionTier::TrustedLocal, false).await;
    assert_eq!(
        mode,
        AttachMode::ReadWrite,
        "K08 old client did not reserve writer registration"
    );
    let mut peer = K08Observer::attach(&service, ConnectionTier::Peer).await;
    let mut local = K08Observer::attach(&service, ConnectionTier::TrustedLocal).await;
    let mut local_view = seed_conversation(&local.initial);
    let mut sender = new_sender(vec![host.peer("k08")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation k08 K08 approved question",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let roots = received(&room_events(&host.journal).await);
    let approved_task = roots[0].1.clone();
    let approved_node = waiting_node(&host, &approved_task).await;
    write_frame(
        &mut writer,
        &ClientFrame::ResolvePeerDraft {
            node: approved_node.clone(),
            action: PeerDraftAction::Approve,
        },
    )
    .await
    .unwrap();
    let (_, _, outcome) = recipient_action_result(&mut writer, None).await;
    assert!(
        matches!(outcome, RecipientActionOutcome::Applied { reply: Some(_) }),
        "K08 real signed approved-draft action outcome: {outcome:?}"
    );
    let _ = history_fence(&mut writer).await;
    let terminal = poll_state(&host.endpoint, &approved_task, "completed").await;
    assert_eq!(terminal["result"]["status"]["state"], "completed");

    // The real effective policy overrides consultation to Draft, while its
    // individual default remains Auto for status_request. Both acts therefore
    // share one host, one transcript, and the continuously attached observers.
    sender
        .type_command(
            &mut state,
            "/team send --type status_request k08 K08 automatic question",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let roots = received(&room_events(&host.journal).await);
    let automatic_task = roots
        .iter()
        .find(|(_, task, _)| task != &approved_task)
        .expect("K08 second real A2A request received")
        .1
        .clone();
    let terminal = poll_state(&host.endpoint, &automatic_task, "completed").await;
    assert_eq!(terminal["result"]["status"]["state"], "completed");
    let automatic_node = room_events(&host.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::PeerDisclosure {
                node,
                task: Some(task),
                ..
            } if task == automatic_task => Some(node.as_str().to_owned()),
            _ => None,
        })
        .expect("K08 completed automatic task records actual disclosure node");

    let marker = format!("K08-domain-barrier-{}", host.journal.path.display());
    host.domain_tx
        .send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: marker.clone(),
        })
        .expect("K08 production domain forwarder remains open");
    peer.barrier(&marker).await;
    local.barrier(&marker).await;
    k08_assert_raw_receipts(&peer, &approved_node, &automatic_node);
    k08_assert_raw_receipts(&local, &approved_node, &automatic_node);
    assert!(
        !peer
            .frames
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::PeerRowOperation(_))),
        "K08 Peer observer must receive zero live peer operations"
    );
    for node in [&approved_node, &automatic_node] {
        let id = format!("peer-response-{node}");
        assert!(
            local.frames.iter().any(|frame| matches!(frame,
            DaemonFrame::PeerRowOperation(operation) if matches!(operation.as_ref(),
                PeerRowOperation::Upsert { message, .. } if message.id == id))),
            "K08 non-zero trusted-local live Upsert receipt precondition for {id}"
        );
    }
    for frame in &local.frames {
        if let DaemonFrame::PeerRowOperation(operation) = frame {
            apply_peer_row_operation(&mut local_view, operation.as_ref().clone());
        }
    }
    k08_assert_rows(
        &local_view.messages,
        &approved_node,
        &automatic_node,
        false,
        "local live Upsert",
    );
    let peer_history = peer.history().await;
    let local_history = local.history().await;
    k08_assert_rows(
        &peer_history,
        &approved_node,
        &automatic_node,
        true,
        "Peer History",
    );
    k08_assert_rows(
        &local_history,
        &approved_node,
        &automatic_node,
        false,
        "local History",
    );
    assert!(
        !peer
            .frames
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::PeerRowOperation(_))),
        "K08 Peer history fence also receives zero peer operations"
    );
    let (_peer_fresh, _, peer_snapshot) = service.attach(ConnectionTier::Peer, true).await;
    let (_local_fresh, _, local_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, true).await;
    k08_assert_rows(
        &peer_snapshot.transcript,
        &approved_node,
        &automatic_node,
        true,
        "Peer snapshot",
    );
    k08_assert_rows(
        &local_snapshot.transcript,
        &approved_node,
        &automatic_node,
        false,
        "local snapshot",
    );
    for (surface, rows) in [
        ("live", &local_view.messages),
        ("History", &local_history),
        ("snapshot", &local_snapshot.transcript),
    ] {
        let auto_id = format!("peer-response-{automatic_node}");
        let auto_row = rows.iter().find(|row| row.id == auto_id).unwrap().clone();
        let conversation = Conversation {
            messages: vec![auto_row],
            ..Default::default()
        };
        let lines = paint(&conversation).join("\n");
        assert!(
            lines.contains("[agent-composed]")
                && lines.contains("[auto-sent]")
                && lines.contains("[✗] Retract"),
            "K08 local {surface} renders automatic facts through parent paint: {lines}"
        );
    }
    host.cancel.cancel();
    host.http.await.unwrap().unwrap();
}
