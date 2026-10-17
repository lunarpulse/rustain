use crate::attached_harness::{AttachedClient, AttachedDaemon};
use crate::g_harness::*;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, TeamBoardRequest,
};
use rustain::domain::models::{JournalRecord, PermissionMode, ReplyAuthorship, RoomEvent};
use serde_json::{Value, json};

#[tokio::test]
async fn model_delegation_through_attached_turn_is_agent_composed() {
    let host = crate::attached_harness::RecipientHost::start(
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
    let name = tokio::time::timeout(crate::attached_harness::FIXTURE_TIMEOUT, async {
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
        [ReplyAuthorship::AgentComposed],
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
        crate::harness::read_room_events(&host.journal)
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
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    for observer in [&mut writer, &mut readonly, &mut peer] {
        let dispatch=observer.wait_for(|frame|matches!(frame,DaemonFrame::Event(event)
            if matches!(&event.kind,rustain::infrastructure::runtime::event_bus::RawEventKind::DomainEvent(
                rustain::domain::events::DomainEventPayload::Room(RoomEvent::RemoteEnvelopeDispatched {..}))))).await;
        assert!(
            matches!(dispatch,DaemonFrame::Event(event)
            if matches!(event.kind,rustain::infrastructure::runtime::event_bus::RawEventKind::DomainEvent(
                rustain::domain::events::DomainEventPayload::Room(RoomEvent::RemoteEnvelopeDispatched {
                    authorship:ReplyAuthorship::Unknown,..})))),
            "M49 attached: journal-only dispatch authorship must not leak on raw frames"
        );
    }
    fence(&mut writer, "raw-writer-fence").await;
    fence(&mut peer, "raw-peer-fence").await;
    fence(&mut readonly, "raw-readonly-fence").await;
    let entries = daemon.journal.entries().await;
    assert!(
        entries.iter().any(|entry| matches!(
            &entry.record,
            JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched {
                authorship: ReplyAuthorship::AgentComposed,
                ..
            })
        )),
        "positive control: journal retains approved authorship"
    );
    for frame in writer
        .received
        .iter()
        .chain(&peer.received)
        .chain(&readonly.received)
    {
        if let DaemonFrame::Event(event) = frame {
            let raw = serde_json::to_string(event).unwrap();
            assert!(
                !raw.contains("agent-composed"),
                "M49 attached: journal-only dispatch authorship must not leak on raw frames"
            );
        }
    }
    assert!(
        !peer
            .received
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamProposal { .. })),
        "M50 attached privacy: daemon registry must not publish proposal to Peer"
    );
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
                authorship: ReplyAuthorship::Unknown,
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
