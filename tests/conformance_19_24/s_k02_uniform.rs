//! Story 19.24: declining a status request must leave an indistinguishable reply.
use super::d_k04_render::StoryRecipient;
use rustain::adapters::daemon::protocol::{ClientFrame, DaemonFrame};
use rustain::domain::models::{JournalRecord, PeerRowStage, ResponseMode, RoomEvent};
use serde_json::json;

#[tokio::test]
async fn s_k02_decline_records_nothing_instead_of_silence() {
    let mut recipient = StoryRecipient::start_status(ResponseMode::NotifyAndWait, "minimal").await;
    let result = recipient
        .sender
        .message_send_with_metadata(
            "status-decline",
            "Give me a sprint progress report",
            json!({"x-rustain-message-type": "status_request"}),
        )
        .await;
    assert!(result.json["error"].is_null(), "{result:?}");
    recipient
        .daemon
        .wait_for_received_task("status-decline")
        .await;
    let address = recipient
        .daemon
        .journal
        .entries()
        .await
        .into_iter()
        .find_map(|entry| match entry.record {
            JournalRecord::Room(RoomEvent::RecipientItemReceived { address, task, .. })
                if task == "status-decline" =>
            {
                Some(address)
            }
            _ => None,
        })
        .expect("status request arrived through HTTP");
    recipient
        .wait_control(&address, PeerRowStage::AwaitingOperator)
        .await;
    recipient
        .client
        .send_frame(ClientFrame::ShareRecipientStatus {
            item_id: address.item().as_str().to_owned(),
            decline: true,
        })
        .await;
    recipient
        .client
        .wait_for(|frame| matches!(frame, DaemonFrame::RecipientActionResult { .. }))
        .await;
    let replies = recipient
        .daemon
        .journal
        .entries()
        .await
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(event @ RoomEvent::RecipientItemReplied { .. }) => {
                Some(serde_json::to_value(event).expect("journal JSON"))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        replies.len(),
        1,
        "decline must append one Status turn, not disappear"
    );
    assert_eq!(replies[0]["status"], json!({"stories": []}));
    recipient.stop().await;
}

#[tokio::test]
async fn s_k02_unanswered_status_board_does_not_invent_a_report() {
    use super::harness::{Host, Sender};
    use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
    use rustain::adapters::tui::handlers::team_command::TeamBoardSelector;
    use rustain::adapters::tui::state::TuiState;
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![host.peer("member")]).await;
    let mut state = TuiState::new(80, 24);
    sender
        .command(
            &mut state,
            "/team send --type status_request member Give me a sprint progress report",
        )
        .await;
    sender.settle(&mut state, 1).await;
    let board = rustain::infrastructure::runtime::transparency_bridge::team_board_message(
        sender.runtime(),
        &TeamBoardSelector::Last,
    )
    .await;
    assert!(board.contains("awaiting reply"), "{board}");
    assert!(!board.contains("no status shared"), "{board}");
    host.stop().await;
}

#[tokio::test]
async fn s_k02_decline_none_and_empty_are_identical_on_the_served_wire() {
    use super::s_harness::{act, request};
    use rustain::adapters::daemon::protocol::RecipientActionOutcome;
    let mut empty = Vec::new();
    for (level, holds_story, decline) in [
        ("minimal", true, true),
        ("none", true, false),
        ("minimal", false, false),
        ("minimal", true, false),
    ] {
        let mut recipient = StoryRecipient::start_status(ResponseMode::NotifyAndWait, level).await;
        if holds_story {
            let assignment = recipient
                .send(
                    "holding",
                    "private assignment",
                    Some(json!({"stories":[{"key":"S7-03","you":true}]})),
                )
                .await;
            recipient
                .wait_control(&assignment, PeerRowStage::Settled)
                .await;
        }
        let item = request(
            &mut recipient,
            "wire-status",
            "status please",
            "status_request",
            PeerRowStage::AwaitingOperator,
        )
        .await;
        assert!(matches!(
            act(&mut recipient.client, &item, decline).await,
            RecipientActionOutcome::Applied { .. }
        ));
        let wire = recipient
            .sender
            .rpc(
                "x-rustain-items/thread",
                json!({"itemId":item.item().as_str()}),
            )
            .await;
        assert!(wire["error"].is_null(), "{wire}");
        let turns = wire["result"]["turns"]
            .as_array()
            .expect("real served thread");
        let replies = turns
            .iter()
            .filter(|turn| turn["kind"] == "reply")
            .collect::<Vec<_>>();
        assert_eq!(replies.len(), 1, "{wire}");
        let reply = replies[0];
        assert_eq!(reply["replyKind"], "status");
        assert_eq!(reply["authorship"], "agent-composed");
        let encoded = serde_json::to_string(reply).unwrap();
        for forbidden in [
            "nothingBecause",
            "nothing_because",
            "sendAuthorization",
            "send_authorization",
            "private assignment",
        ] {
            assert!(!encoded.contains(forbidden), "{encoded}");
        }
        let payload = (reply["status"].clone(), reply["content"].clone());
        if decline || level == "none" || !holds_story {
            assert_eq!(payload.0, json!({"stories":[]}));
            assert_eq!(payload.1, "No status shared.");
            empty.push(payload);
        } else {
            assert_eq!(
                payload.0,
                json!({"level":"minimal","stories":[{"key":"S7-03"}]})
            );
            assert_eq!(payload.1, "Status (minimal): S7-03");
        }
        recipient.stop().await;
    }
    assert_eq!(empty.len(), 3);
    assert!(empty.windows(2).all(|pair| pair[0] == pair[1]));
}

#[tokio::test]
async fn s_k02_status_board_keeps_empty_decline_unanswered_and_busy_distinct_only_as_allowed() {
    use super::attached_harness::{AttachedClient, AttachedDaemon, RecipientHost};
    use super::s_harness::{act, host_item, status_policy, wait_host_control};
    use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
    use rustain::adapters::daemon::protocol::{
        ConnectionTier, RecipientAction, RecipientActionOutcome, TeamBoardRequest, TeamSendOutcome,
    };
    use rustain::domain::models::{SemanticMessageType, StoryManifest};

    let mut hosts = Vec::new();
    let mut operators = Vec::new();
    for (alias, level) in [
        ("b", "minimal"),
        ("d", "minimal"),
        ("e", "none"),
        ("f", "minimal"),
        ("g", "minimal"),
        ("h", "minimal"),
    ] {
        let mut host = RecipientHost::start_with_effective_policy(
            A2aAdmissionPolicy::Allow,
            status_policy(level),
        )
        .await;
        let path = host.enable_operator().await;
        operators.push(AttachedClient::attach(&path, ConnectionTier::TrustedLocal, false).await);
        hosts.push((alias, host));
    }
    let denied = RecipientHost::start(A2aAdmissionPolicy::Deny).await;
    let mut peers = hosts
        .iter()
        .map(|(alias, host)| host.peer(alias))
        .collect::<Vec<_>>();
    peers.push(denied.peer("i"));
    let sender = AttachedDaemon::start(peers).await;
    let mut writer =
        AttachedClient::attach(&sender.path, ConnectionTier::TrustedLocal, false).await;
    writer
        .send_team(
            "matrix-assignment",
            hosts.iter().map(|(alias, _)| (*alias).to_owned()).collect(),
            "private assignment",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(StoryManifest::parse("S7-03=b,S7-04=d,S7-05=e,S7-06=g,S7-07=h").unwrap()),
        )
        .await;
    for index in 0..hosts.len() {
        assert!(matches!(
            writer.wait_send_settled("matrix-assignment", index).await,
            DaemonFrame::TeamSendSettled {
                outcome: TeamSendOutcome::Delivered,
                ..
            }
        ));
    }
    writer
        .send_team(
            "matrix-status",
            ["b", "d", "e", "f", "g", "h", "i"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            "status please",
            Some(SemanticMessageType::StatusRequest),
            None,
            None,
        )
        .await;
    for index in 0..7 {
        let settled = writer.wait_send_settled("matrix-status", index).await;
        if index == 6 {
            assert!(
                matches!(
                    settled,
                    DaemonFrame::TeamSendSettled {
                        outcome: TeamSendOutcome::Declined,
                        ..
                    }
                ),
                "{settled:?}"
            );
        } else {
            assert!(
                matches!(
                    settled,
                    DaemonFrame::TeamSendSettled {
                        outcome: TeamSendOutcome::Delivered,
                        ..
                    }
                ),
                "{settled:?}"
            );
        }
    }
    for ((alias, host), client) in hosts.iter().zip(&mut operators) {
        let item = host_item(host, SemanticMessageType::StatusRequest).await;
        let control = wait_host_control(client, &item).await;
        assert!(control.status.is_some());
        match *alias {
            "b" | "d" | "e" | "f" => assert!(matches!(
                act(client, &item, *alias == "d").await,
                RecipientActionOutcome::Applied { .. }
            )),
            "h" => {
                client
                    .send_frame(ClientFrame::DeferRecipientItem {
                        item_id: item.item().as_str().to_owned(),
                    })
                    .await;
                let deferred = client
                    .wait_for(|frame| {
                        matches!(
                            frame,
                            DaemonFrame::RecipientActionResult {
                                action: RecipientAction::Defer,
                                ..
                            }
                        )
                    })
                    .await;
                assert!(
                    matches!(
                        deferred,
                        DaemonFrame::RecipientActionResult {
                            outcome: RecipientActionOutcome::Applied { .. },
                            ..
                        }
                    ),
                    "{deferred:?}"
                );
            }
            "g" => {}
            _ => unreachable!(),
        }
    }
    writer
        .team_board("matrix-board", TeamBoardRequest::Last)
        .await;
    let board = writer.wait_board_result("matrix-board").await.unwrap();
    let unwrapped = board.replace("\n      ", "");
    let row = |alias: &str| {
        unwrapped
            .lines()
            .find(|line| line.starts_with(&format!("  {alias} ")))
            .unwrap_or_else(|| panic!("missing {alias}: {board}"))
    };
    assert!(row("b").contains("S7-03"), "{board}");
    assert!(
        row("b").contains("Source: manually shared (b's policy: minimal) [agent-composed]"),
        "{board}"
    );
    for alias in ["d", "e", "f", "i"] {
        assert!(row(alias).contains("no status shared"), "{board}");
        assert!(row(alias).contains("Source: —"), "{board}");
        assert!(!row(alias).contains("declined"), "{board}");
    }
    assert!(row("g").contains("awaiting reply"), "{board}");
    assert!(!row("g").contains("no status shared"), "{board}");
    assert!(
        row("h").contains("awaiting reply [defer 30m — reported expectation]"),
        "{board}"
    );
    for (_, host) in &hosts {
        assert!(host.requests.lock().is_empty());
        assert_eq!(host.method_count("x-rustain-items/thread"), 1);
    }
    assert_eq!(denied.method_count("x-rustain-items/thread"), 0);
    writer.detach().await;
    for client in operators {
        client.detach().await;
    }
    sender.stop().await;
    for (_, host) in hosts {
        host.stop().await;
    }
    denied.stop().await;
}
