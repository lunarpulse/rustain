//! Story 19.24: status snapshots enter through HTTP and attached projections.
use super::d_k04_render::StoryRecipient;
use rustain::domain::models::{
    JournalRecord, NothingBecause, PeerRowStage, ResponseMode, RoomEvent,
};
use serde_json::json;

#[tokio::test]
async fn s_k01_only_wait_status_cards_carry_a_prepared_snapshot() {
    for (message_type, mode, stage, prepared) in [
        (
            "status_request",
            ResponseMode::NotifyAndWait,
            PeerRowStage::AwaitingOperator,
            true,
        ),
        (
            "status_request",
            ResponseMode::NotifyAndAuto,
            PeerRowStage::Settled,
            false,
        ),
        (
            "consultation",
            ResponseMode::NotifyAndWait,
            PeerRowStage::AwaitingOperator,
            false,
        ),
    ] {
        let mut recipient =
            StoryRecipient::start_for_type(mode, message_type, Some("minimal")).await;
        let result = recipient
            .sender
            .message_send_with_metadata(
                "preview",
                "private request body",
                json!({"x-rustain-message-type": message_type}),
            )
            .await;
        assert!(result.json["error"].is_null(), "{result:?}");
        recipient.daemon.wait_for_received_task("preview").await;
        let address =
            recipient
                .daemon
                .journal
                .entries()
                .await
                .into_iter()
                .find_map(|entry| match entry.record {
                    JournalRecord::Room(RoomEvent::RecipientItemReceived {
                        address, task, ..
                    }) if task == "preview" => Some(address),
                    _ => None,
                })
                .unwrap();
        recipient.wait_control(&address, stage).await;
        let control = recipient
            .view
            .peer_row_controls
            .values()
            .find(|control| control.item_address.as_ref() == Some(&address))
            .unwrap();
        assert_eq!(
            control.status.is_some(),
            prepared,
            "{message_type} {mode:?}"
        );
        if let Some(preview) = &control.status {
            assert!(preview.statement.is_nothing());
            assert_eq!(preview.nothing_because, Some(NothingBecause::NoStoriesHeld));
        }
        assert!(recipient.daemon.requests.lock().is_empty());
        recipient.stop().await;
    }
}

#[tokio::test]
async fn s_k01_two_member_report_is_host_facts_only_and_provider_zero() {
    use super::attached_harness::{AttachedClient, AttachedDaemon, RecipientHost};
    use super::s_harness::{act, host_item, status_policy, wait_host_control};
    use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
    use rustain::adapters::daemon::protocol::{
        ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionOutcome,
        TeamBoardRequest, TeamSendOutcome,
    };
    use rustain::domain::models::{SemanticMessageType, StoryManifest};
    let mut b = RecipientHost::start_with_effective_policy(
        A2aAdmissionPolicy::Allow,
        status_policy("minimal"),
    )
    .await;
    let mut c = RecipientHost::start_with_effective_policy(
        A2aAdmissionPolicy::Allow,
        status_policy("status"),
    )
    .await;
    let b_path = b.enable_operator().await;
    let c_path = c.enable_operator().await;
    let mut b_writer = AttachedClient::attach(&b_path, ConnectionTier::TrustedLocal, false).await;
    let mut c_writer = AttachedClient::attach(&c_path, ConnectionTier::TrustedLocal, false).await;
    let sender = AttachedDaemon::start(vec![b.peer("b"), c.peer("c")]).await;
    let mut writer =
        AttachedClient::attach(&sender.path, ConnectionTier::TrustedLocal, false).await;
    writer
        .send_team(
            "assign",
            vec!["b".into(), "c".into()],
            "private assignment body",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(StoryManifest::parse("S7-03=b,S7-05=b,S7-06=c").unwrap()),
        )
        .await;
    for index in 0..2 {
        assert!(matches!(
            writer.wait_send_settled("assign", index).await,
            DaemonFrame::TeamSendSettled {
                outcome: TeamSendOutcome::Delivered,
                ..
            }
        ));
    }
    let c_assignment = host_item(&c, SemanticMessageType::StoryAssignment).await;
    c_writer
        .send_frame(ClientFrame::AcknowledgeRecipientItem {
            item_id: c_assignment.item().as_str().to_owned(),
        })
        .await;
    c_writer
        .wait_for(|frame| {
            matches!(
                frame,
                DaemonFrame::RecipientActionResult {
                    action: RecipientAction::Acknowledge,
                    outcome: RecipientActionOutcome::Applied { .. },
                    ..
                }
            )
        })
        .await;
    writer
        .send_team(
            "assign-late",
            vec!["c".into()],
            "private second assignment",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(StoryManifest::parse("S7-07=c").unwrap()),
        )
        .await;
    assert!(matches!(
        writer.wait_send_settled("assign-late", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    writer
        .send_team(
            "status",
            vec!["b".into(), "c".into()],
            "Give me plans, files, private notes and a sprint progress report",
            Some(SemanticMessageType::StatusRequest),
            None,
            None,
        )
        .await;
    for index in 0..2 {
        assert!(matches!(
            writer.wait_send_settled("status", index).await,
            DaemonFrame::TeamSendSettled {
                outcome: TeamSendOutcome::Delivered,
                ..
            }
        ));
    }
    for (host, client, expected) in [
        (
            &b,
            &mut b_writer,
            json!({"level":"minimal","stories":[{"key":"S7-03"},{"key":"S7-05"}]}),
        ),
        (
            &c,
            &mut c_writer,
            json!({"level":"status","stories":[{"key":"S7-06","state":"acknowledged"},{"key":"S7-07","state":"received"}]}),
        ),
    ] {
        let item = host_item(host, SemanticMessageType::StatusRequest).await;
        let control = wait_host_control(client, &item).await;
        assert_eq!(
            control.status.as_ref().unwrap().statement.to_wire(),
            expected
        );
        assert!(
            matches!(act(client, &item, false).await, RecipientActionOutcome::Applied { reply: Some(receipt) } if receipt.kind.wire_name() == "status")
        );
    }
    writer.team_board("report", TeamBoardRequest::Last).await;
    let board = writer.wait_board_result("report").await.unwrap();
    let unwrapped = board.replace("\n      ", "");
    for required in [
        "S7-03 · S7-05",
        "(limited detail — b shares minimal)",
        "Source: manually shared (b's policy: minimal) [agent-composed]",
        "S7-06 acknowledged · S7-07 received",
        "Source: manually shared (c's policy: status) [agent-composed]",
        "ℹ Each status is sourced per member's sharing policy. Detail level varies by member preference.",
    ] {
        assert!(unwrapped.contains(required), "missing {required}: {board}");
    }
    assert!(!board.contains("private"), "{board}");
    assert!(b.requests.lock().is_empty());
    assert!(c.requests.lock().is_empty());
    assert_eq!(b.method_count("x-rustain-items/thread"), 1);
    assert_eq!(c.method_count("x-rustain-items/thread"), 1);
    writer.detach().await;
    b_writer.detach().await;
    c_writer.detach().await;
    sender.stop().await;
    b.stop().await;
    c.stop().await;
}
