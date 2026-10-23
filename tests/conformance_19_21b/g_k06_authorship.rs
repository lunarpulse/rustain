use super::attached_harness::{AttachedClient, AttachedDaemon};
use super::g_harness::*;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, TeamBoardRequest, ToolInputWithheld,
};
use rustain::domain::models::{
    Authorship, JournalRecord, PermissionMode, RoomEvent, SendAuthorization,
};
use serde_json::{Value, json};

#[tokio::test]
async fn model_delegation_through_attached_turn_is_agent_composed() {
    let host = super::attached_harness::RecipientHost::start(
        rustain::adapters::a2a::admission::A2aAdmissionPolicy::Allow,
    )
    .await;
    // Seed the recipient's real registry before discovery; production signed
    // AgentCard projection and provider discovery install the model-facing name.
    let _registration = host
        .registry
        .register(
            rustain::domain::models::capability_registry::RegisteredCapability {
                id: rustain::domain::models::CapabilityId {
                    protocol: "skill".into(),
                    server: "local".into(),
                    tool: "delegate".into(),
                },
                protocol: "skill".into(),
                provider_id: "fixture-skill".into(),
                name: "delegate".into(),
                description: "Handle a delegated peer task through the production inbound runtime."
                    .into(),
                input_schema: json!({"type":"object"}),
                parallel_safe: false,
                trust: rustain::domain::models::TrustTier::Verified,
            },
        )
        .await
        .expect("recipient skill registration");
    let daemon = AttachedDaemon::start(vec![host.peer("lena-po")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    daemon.runtime.security.set_mode(PermissionMode::Yolo);
    let name = tokio::time::timeout(super::attached_harness::FIXTURE_TIMEOUT, async {
        loop {
            if let Some(tool) = daemon
                .runtime
                .tools
                .available_tools()
                .into_iter()
                .find(|tool| tool.name == "a2a__lena-po__delegate")
            {
                break tool.name;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real egress discovery installs signed delegation capability");
    daemon.script.tool(
        &name,
        json!({"message":"model delegation authorship control"}),
    );
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "run the model delegation control".into(),
            images: vec![],
        })
        .await;
    completed(&mut writer).await;
    let entries = daemon.journal.entries().await;
    let authorship: Vec<_> = entries
        .iter()
        .filter_map(|entry| match &entry.record {
            JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { authorship, .. }) => {
                Some(*authorship)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        authorship,
        [Authorship::AgentComposed],
        "M47 attached: model delegation dispatch must be agent-composed"
    );
    let log = rustain::domain::services::transparency::fold_transparency(&entries)
        .iter()
        .map(|row| row.one_line())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        log.contains("[agent-composed]"),
        "M47 attached: shared team log must tag model delegation agent-composed"
    );
    assert_eq!(
        super::harness::read_room_events(&host.journal)
            .await
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { .. }))
            .count(),
        1,
        "actual model delegation must reach real recipient"
    );
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn approved_dispatch_authorship_stays_out_of_raw_frames() {
    let (daemon, mut writer, host) = fixture().await;
    let mut peer = AttachedClient::attach(&daemon.path, ConnectionTier::Peer, true).await;
    let mut readonly =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    use rustain::domain::events::AppEvent;
    use rustain::domain::models::{StreamChunk, ToolCall};
    use rustain::infrastructure::runtime::event_bus::RawEventKind;
    const SECRET: &str = "R1-undecided-body-private-marker-9e4f";
    const PUBLIC: &str = "R1-unrelated-tool-public-control";
    let body = format!("{SECRET}\n{BODY}");
    let mut scheduler = daemon.runtime.tool_scheduler.subscribe();
    let model_result = submit(
        &daemon,
        &mut writer,
        json!({"recipients":["lena-po"],"type":"architecture_update","body":body}),
    )
    .await;
    assert_eq!(
        model_result,
        "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to lena-po.",
        "private raw projection must not alter the model-facing result"
    );
    let id = match writer
        .wait_for(|frame| matches!(frame, DaemonFrame::TeamProposal { .. }))
        .await
    {
        DaemonFrame::TeamProposal {
            proposal,
            recipients,
            message_type,
            body: snapshot,
            ..
        } => {
            assert_eq!(
                recipients,
                ["lena-po"],
                "writer keeps exact recipient order"
            );
            assert_eq!(
                message_type,
                rustain::domain::models::SemanticMessageType::ArchitectureUpdate
            );
            assert_eq!(
                snapshot.as_bytes(),
                body.as_bytes(),
                "writer keeps exact proposal bytes"
            );
            proposal
        }
        _ => unreachable!(),
    };
    let mut terminal = false;
    while let Ok(transition) = scheduler.try_recv() {
        assert_eq!(
            transition.call.request().input["body"],
            body,
            "scheduler trace must originate from the real private tool invocation"
        );
        terminal |= matches!(&transition.call, ToolCall::Success { result, .. }
            if result.output == model_result);
        // Exercise the same canonical bridge carrier used by standalone turns
        // with genuine attached scheduler transitions, not fabricated payloads.
        daemon
            .domain_tx
            .send(AppEvent::ToolCallTransitionBridged {
                conversation_id: transition.conversation_id.clone(),
                transition,
            })
            .unwrap();
    }
    assert!(
        terminal,
        "production attached scheduler must finish the proposal call"
    );
    daemon.runtime.security.set_mode(PermissionMode::Yolo);
    let public_path = daemon
        .journal
        .workspace
        .path()
        .join("raw-observer-control.txt");
    std::fs::write(&public_path, PUBLIC).unwrap();
    daemon.script.tool("Read", json!({"file_path":public_path}));
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "Read the observer positive-control file.".into(),
            images: vec![],
        })
        .await;
    completed(&mut writer).await;
    for observer in [&mut readonly, &mut peer] {
        completed(observer).await;
        completed(observer).await;
    }
    {
        let requests = daemon.requests.lock();
        for (messages, _) in requests.iter() {
            for (index, message) in messages.iter().enumerate() {
                for call in &message.tool_uses {
                    assert!(
                        messages.get(index + 1).is_some_and(|next| next
                            .tool_results
                            .iter()
                            .any(|result| result.tool_use_id == call.id)),
                        "retained proposal history must keep tool responses adjacent on the next real model turn"
                    );
                }
            }
        }
    }
    let private_tool = {
        let conversation = daemon.conversation.lock().await;
        conversation
            .messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .find(|tool| tool.name == "propose_team_message")
            .cloned()
            .expect("A6 internal transcript retains private tool call")
    };
    assert_eq!(private_tool.input["body"], body);
    assert_eq!(
        private_tool.result.as_ref().unwrap().content,
        model_result,
        "A6 internal transcript retains exact private tool result"
    );
    for tier in [ConnectionTier::Peer, ConnectionTier::TrustedLocal] {
        let mut late = AttachedClient::attach(&daemon.path, tier, true).await;
        let snapshot = late
            .received
            .iter()
            .find_map(|frame| match frame {
                DaemonFrame::AttachAck { snapshot, .. } => Some(snapshot),
                _ => None,
            })
            .expect("late observer must inspect actual signed-socket attach snapshot");
        let encoded = serde_json::to_string(&snapshot.transcript).unwrap();
        assert!(
            !encoded.contains(SECRET) && !encoded.contains("lena-po") && !encoded.contains(&id),
            "late public transcript must not disclose retained private tool arguments/results"
        );
        assert!(
            encoded.contains(PUBLIC),
            "late transcript retains unrelated tool evidence"
        );
        late.send_frame(ClientFrame::HistoryRequest {
            before_index: None,
            count: usize::MAX,
        })
        .await;
        let history = late
            .wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
            .await;
        let encoded = serde_json::to_string(&history).unwrap();
        assert!(
            !encoded.contains(SECRET) && !encoded.contains("lena-po") && !encoded.contains(&id),
            "public history pages must not disclose retained private tool arguments/results"
        );
        assert!(
            encoded.contains(PUBLIC),
            "history pages retain unrelated tool evidence"
        );
    }
    let before_send = daemon.journal.entries().await.len();
    send_once(&daemon, &mut writer, &host, &id).await;
    let posts = host.posts();
    let sent: Value = serde_json::from_slice(posts[0].body()).unwrap();
    assert_eq!(
        sent.pointer("/params/message/parts/0/text")
            .and_then(Value::as_str),
        Some(body.as_str()),
        "approved send remains byte-exact"
    );
    assert_eq!(
        sent.pointer("/params/message/metadata/x-rustain-message-type")
            .and_then(Value::as_str),
        Some("architecture_update")
    );
    for observer in [&mut writer, &mut readonly, &mut peer] {
        let dispatch=observer.wait_for(|frame|matches!(frame,DaemonFrame::Event(event)
            if matches!(&event.kind,rustain::infrastructure::runtime::event_bus::RawEventKind::DomainEvent(
                rustain::domain::events::DomainEventPayload::Room(RoomEvent::RemoteEnvelopeDispatched {..}))))).await;
        assert!(
            matches!(dispatch,DaemonFrame::Event(event)
            if matches!(event.kind,rustain::infrastructure::runtime::event_bus::RawEventKind::DomainEvent(
                rustain::domain::events::DomainEventPayload::Room(RoomEvent::RemoteEnvelopeDispatched {
                    authorship:Authorship::Unknown,..})))),
            "M49 attached: journal-only dispatch authorship must not leak on raw frames"
        );
    }
    fence(&mut writer, "raw-writer-fence").await;
    fence(&mut peer, "raw-peer-fence").await;
    fence(&mut readonly, "raw-readonly-fence").await;
    let entries = daemon.journal.entries().await;
    let dispatches: Vec<_> = entries[before_send..]
        .iter()
        .filter(|entry| {
            matches!(
                &entry.record,
                JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
            )
        })
        .collect();
    assert_eq!(
        dispatches.len(),
        1,
        "approved proposal produces exactly one dispatch"
    );
    let dispatch = dispatches[0];
    assert!(
        matches!(
            &dispatch.record,
            JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched {
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::HumanDecided,
                ..
            })
        ),
        "R6 approved proposal dispatch retains AgentComposed + HumanDecided"
    );
    let row = rustain::domain::services::transparency::transparency_row(dispatch).unwrap();
    assert!(
        row.summary.contains("[agent-composed]"),
        "R6 approved proposal summary retains agent-composed: {}",
        row.summary
    );
    assert!(
        !row.summary.contains("[auto-sent]"),
        "R6 approved proposal summary must not claim auto-sent: {}",
        row.summary
    );
    for frame in writer
        .received
        .iter()
        .chain(&peer.received)
        .chain(&readonly.received)
    {
        // Story 19.20c: Peer-tier tool events arrive as `ToolEventWithheld`;
        // both carriers are raw frames and both must stay private.
        let event = match frame {
            DaemonFrame::Event(event) | DaemonFrame::ToolEventWithheld { event, .. } => event,
            _ => continue,
        };
        let raw = serde_json::to_string(frame).unwrap();
        assert!(
            !raw.contains(SECRET) && !raw.contains(&id),
            "M50 attached privacy: no raw carrier may disclose undecided proposal body or id"
        );
        if matches!(
            &event.kind,
            RawEventKind::Provider(StreamChunk::ToolUse { .. } | StreamChunk::ToolResult { .. })
                | RawEventKind::PeerProvider(
                    StreamChunk::ToolUse { .. } | StreamChunk::ToolResult { .. }
                )
                | RawEventKind::Tool(_)
        ) {
            assert!(
                !raw.contains("lena-po"),
                "M50 attached privacy: private tool inputs/results/transitions hide proposal aliases"
            );
        }
        assert!(
            !raw.contains("agent-composed"),
            "M49 attached: journal-only dispatch authorship must not leak on raw frames"
        );
    }
    assert!(
        !peer
            .received
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamProposal { .. })),
        "M50 attached privacy: daemon registry must not publish proposal to Peer"
    );
    // Trusted-local attachments keep the unrelated tool invocation with its
    // input (FR29); Peer tier sees the same invocation with its arguments
    // withheld (Story 19.20c owner ruling O1).
    for observer in [&writer, &readonly] {
        assert!(
            observer.received.iter().any(|frame| matches!(frame,
            DaemonFrame::Event(event) if matches!(&event.kind,
                RawEventKind::Provider(StreamChunk::ToolUse { name, .. }) if name == "Read"))),
            "unrelated tool invocation must remain observable on every trusted attachment"
        );
    }
    assert!(
        peer.received.iter().any(|frame| matches!(frame,
            DaemonFrame::ToolEventWithheld {
                event,
                withheld: ToolInputWithheld::PeerTier,
            } if matches!(&event.kind,
                RawEventKind::Provider(StreamChunk::ToolUse { name, input, .. })
                    if name == "Read" && input.is_null()))),
        "Peer tier observes the unrelated tool invocation with its input withheld"
    );
    for observer in [&writer, &readonly, &peer] {
        assert!(
            observer.received.iter().any(|frame| matches!(frame,
            DaemonFrame::Event(event) if matches!(&event.kind,
                RawEventKind::Provider(StreamChunk::ToolResult { content, .. })
                    if content.contains(PUBLIC)))),
            "unrelated tool result must remain observable on every attachment"
        );
    }
    assert!(
        !readonly
            .received
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamProposal { .. })),
        "M50 attached privacy: read-only clients must not receive proposal DTOs"
    );
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn interrupted_proposal_history_does_not_poison_the_next_model_turn() {
    use rustain::domain::models::StreamChunk;

    let (daemon, mut writer, host) = fixture().await;
    daemon.script.exchange(vec![StreamChunk::ToolUse {
        id: "interrupted-proposal-call".into(),
        name: "propose_team_message".into(),
        input: json!({
            "recipients":["lena-po"], "type":"architecture_update",
            "body":"interrupted-private-body"
        }),
    }]);
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "A provider stream will disconnect before this call can execute.".into(),
            images: vec![],
        })
        .await;
    completed(&mut writer).await;
    assert!(daemon.proposals.pending().await.is_empty());
    zero(
        &daemon,
        &[&host],
        "an interrupted tool stream cannot raise or send",
    )
    .await;
    {
        let conversation = daemon.conversation.lock().await;
        let interrupted = conversation
            .messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .find(|tool| tool.id == "interrupted-proposal-call")
            .expect("interrupted invocation remains internal audit evidence");
        assert!(interrupted.result.is_none());
        assert_eq!(interrupted.input["body"], "interrupted-private-body");
    }

    assert!(
        submit(&daemon, &mut writer, input())
            .await
            .starts_with("Proposal shown")
    );
    let id = proposal(&mut writer).await;
    {
        let requests = daemon.requests.lock();
        let (messages, _) = requests.last().unwrap();
        assert!(
            messages.iter().all(|message| message
                .tool_uses
                .iter()
                .all(|call| call.id != "interrupted-proposal-call")
                && message
                    .tool_results
                    .iter()
                    .all(|result| result.tool_use_id != "interrupted-proposal-call")),
            "the next real model request must not contain an unmatched interrupted invocation"
        );
    }
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn future_dispatch_authorship_replays_into_attached_board() {
    let (daemon, mut writer, host) = fixture().await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    let journal = daemon.journal.clone();
    daemon.stop().await;
    let raw = tokio::fs::read_to_string(journal.node.path())
        .await
        .expect("real journal bytes");
    let mut changed = 0;
    let mut rewritten = String::new();
    for line in raw.lines() {
        let mut row: Value = serde_json::from_str(line).expect("journal line");
        if row["record"]["payload"]["event"] == "remote_envelope_dispatched" {
            row["record"]["payload"]["authorship"] = json!("future-attached-authorship");
            changed += 1;
        }
        rewritten.push_str(&serde_json::to_string(&row).unwrap());
        rewritten.push('\n');
    }
    assert_eq!(
        changed, 1,
        "future-token fixture must modify actual approved dispatch"
    );
    tokio::fs::write(journal.node.path(), rewritten)
        .await
        .expect("future token fixture bytes");
    let future_journal =
        rustain::infrastructure::subagent::NodeJournal::open_workspace(journal.workspace.path())
            .await
            .expect("M48 attached: future authorship must replay as Unknown");
    future_journal
        .load()
        .await
        .expect("M48 attached: future authorship must replay as Unknown");
    let reopened = journal.reopen().await;
    let loaded = reopened.entries().await;
    assert!(
        loaded.iter().any(|entry| matches!(
            &entry.record,
            JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched {
                authorship: Authorship::Unknown,
                ..
            })
        )),
        "M48 attached: future authorship must replay as Unknown"
    );
    std::fs::remove_file(journal.workspace.path().join("daemon.sock"))
        .expect("remove stopped fixture socket");
    let restarted =
        AttachedDaemon::start_with_journal(reopened, vec![host.peer("lena-po")], None).await;
    let mut reader =
        AttachedClient::attach(&restarted.path, ConnectionTier::TrustedLocal, false).await;
    reader
        .team_board("future-board", TeamBoardRequest::Last)
        .await;
    let board = reader
        .wait_board_result("future-board")
        .await
        .expect("M48 attached: replayed send must remain readable by attached board");
    assert!(
        board.contains("lena-po"),
        "M48 attached: replayed send must remain readable by attached board"
    );
    restarted.stop().await;
    host.stop().await;
}
