use super::attached_harness::{AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT, RecipientHost};
use super::g_harness::{completed, fence};
use rustain::adapters::daemon::protocol::{ClientFrame, ConnectionTier, DaemonFrame};
use rustain::domain::models::{
    ApprovalOutcome, Authorship, JournalRecord, PermissionMode, RoomEvent, SendAuthorization,
};
use serde_json::json;

#[tokio::test]
async fn g_k20a_delegation_authorization_follows_the_gate() {
    let host =
        RecipientHost::start(rustain::adapters::a2a::admission::A2aAdmissionPolicy::Allow).await;
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
                description: "Delegation gate control".into(),
                input_schema: json!({"type":"object"}),
                parallel_safe: false,
                trust: rustain::domain::models::TrustTier::Verified,
            },
        )
        .await
        .unwrap();
    let daemon = AttachedDaemon::start(vec![host.peer("lena-po")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let mut readonly =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    let mut peer = AttachedClient::attach(&daemon.path, ConnectionTier::Peer, true).await;
    let name = tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            if let Some(tool) = daemon
                .runtime
                .tools
                .available_tools()
                .into_iter()
                .find(|t| t.name == "a2a__lena-po__delegate")
            {
                break tool.name;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("signed discovery installs delegation tool");
    for (index, expected) in [
        SendAuthorization::Automatic,
        SendAuthorization::HumanDecided,
        SendAuthorization::HumanDecided,
        SendAuthorization::Automatic,
    ]
    .into_iter()
    .enumerate()
    {
        daemon.runtime.security.set_mode(if index == 0 {
            PermissionMode::Yolo
        } else {
            PermissionMode::Normal
        });
        daemon
            .script
            .tool(&name, json!({"message":format!("gate control {index}")}));
        writer
            .send_frame(ClientFrame::UserMessage {
                text: format!("delegate gate control {index}"),
                images: vec![],
            })
            .await;
        if index == 1 || index == 2 {
            let request_id = match writer
                .wait_for(|f| matches!(f, DaemonFrame::ApprovalRequest {tool,..} if tool == &name))
                .await
            {
                DaemonFrame::ApprovalRequest { request_id, .. } => request_id,
                _ => unreachable!(),
            };
            writer
                .send_frame(ClientFrame::ApprovalResponse {
                    request_id,
                    outcome: if index == 1 {
                        ApprovalOutcome::Once
                    } else {
                        ApprovalOutcome::AlwaysTool {
                            tool_name: name.clone(),
                        }
                    },
                })
                .await;
        }
        completed(&mut writer).await;
        completed(&mut readonly).await;
        completed(&mut peer).await;
        let entries = daemon.journal.entries().await;
        let dispatches: Vec<_> = entries
            .iter()
            .filter(|e| {
                matches!(
                    e.record,
                    JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
                )
            })
            .collect();
        assert_eq!(
            dispatches.len(),
            index + 1,
            "every model turn must dispatch"
        );
        let entry = dispatches[index];
        let JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched {
            authorship,
            send_authorization,
            ..
        }) = &entry.record
        else {
            unreachable!()
        };
        assert_eq!(*authorship, Authorship::AgentComposed);
        assert_eq!(
            *send_authorization, expected,
            "gate leg {index}: authorization must follow this invocation's resolver"
        );
        let row = rustain::domain::services::transparency::transparency_row(entry).unwrap();
        assert!(row.summary.contains("[agent-composed]"), "{}", row.summary);
        assert_eq!(
            row.summary.contains("[auto-sent]"),
            expected == SendAuthorization::Automatic,
            "gate leg {index}: {}",
            row.summary
        );
        assert_eq!(
            super::harness::read_room_events(&host.journal)
                .await
                .iter()
                .filter(|(_, e)| matches!(e, RoomEvent::RecipientItemReceived { .. }))
                .count(),
            index + 1,
            "same peer receives every real delegation"
        );
    }
    for observer in [&mut writer, &mut readonly, &mut peer] {
        fence(observer, "k20a-raw-fence").await;
        let mut count = 0;
        for frame in &observer.received {
            let event = match frame {
                DaemonFrame::Event(event) | DaemonFrame::ToolEventWithheld { event, .. } => event,
                _ => continue,
            };
            if let rustain::infrastructure::runtime::event_bus::RawEventKind::DomainEvent(
                rustain::domain::events::DomainEventPayload::Room(
                    RoomEvent::RemoteEnvelopeDispatched {
                        authorship,
                        send_authorization,
                        ..
                    },
                ),
            ) = &event.kind
            {
                count += 1;
                assert_eq!(
                    *authorship,
                    Authorship::Unknown,
                    "raw dispatch must hide journal authorship"
                );
                assert_eq!(
                    *send_authorization,
                    SendAuthorization::Unknown,
                    "M28: raw dispatch must hide journal authorization"
                );
            }
        }
        assert_eq!(
            count, 4,
            "each pre-attached tier must observe all real dispatches"
        );
    }
    daemon.stop().await;
    host.stop().await;
}
