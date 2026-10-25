//! Story 19.24: sharing resolves the stored preview once, not a fresh journal fold.
use super::d_k04_render::StoryRecipient;
use super::s_harness::{act, replies, request};
use rustain::adapters::daemon::protocol::RecipientActionOutcome;
use rustain::domain::models::{PeerRowStage, ResponseMode};
use serde_json::json;

#[tokio::test]
async fn s_k03_share_uses_arrival_snapshot_and_rejects_a_different_second_act() {
    let mut recipient = StoryRecipient::start_for_type_with_norm(
        ResponseMode::NotifyAndWait,
        "status_request",
        Some("minimal"),
        Some("status"),
    )
    .await;
    let explanation =
        rustain::domain::services::team_policy::explain_effective_policy(&recipient.policy, &[]);
    let advisory = explanation
        .rows
        .iter()
        .find(|row| row.key == "sharing_breadth")
        .unwrap();
    assert_eq!(
        advisory.detail,
        "sharing breadth: minimal (yours) is narrower than the team norm status (team-policy.toml). Yours applies; the team norm is advisory."
    );
    let first = recipient
        .send(
            "assignment-before",
            "private assignment body",
            Some(json!({"stories":[{"key":"S7-03","you":true}]})),
        )
        .await;
    recipient.wait_control(&first, PeerRowStage::Settled).await;
    let poll = request(
        &mut recipient,
        "status-demand",
        "Report plans, private notes, transcript and all activity",
        "status_request",
        PeerRowStage::AwaitingOperator,
    )
    .await;
    let preview = recipient
        .view
        .peer_row_controls
        .values()
        .find(|control| control.item_address.as_ref() == Some(&poll))
        .unwrap()
        .status
        .as_ref()
        .unwrap()
        .statement
        .to_wire();
    assert_eq!(
        preview,
        json!({"level":"minimal","stories":[{"key":"S7-03"}]})
    );
    let plain = request(
        &mut recipient,
        "status-plain",
        "status please",
        "status_request",
        PeerRowStage::AwaitingOperator,
    )
    .await;
    let late = recipient
        .send(
            "assignment-after",
            "private newer body",
            Some(json!({"stories":[{"key":"S7-05","you":true}]})),
        )
        .await;
    recipient.wait_control(&late, PeerRowStage::Settled).await;
    assert!(
        matches!(act(&mut recipient.client, &poll, false).await, RecipientActionOutcome::Applied { reply: Some(receipt) } if receipt.kind.wire_name() == "status")
    );
    let turns = replies(&recipient, &poll).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0]["status"], preview);
    assert_eq!(turns[0]["content"], "Status (minimal): S7-03");
    assert_eq!(turns[0]["authorship"], "agent-composed");
    assert_eq!(turns[0]["send_authorization"], "human-decided");
    assert!(matches!(
        act(&mut recipient.client, &poll, false).await,
        RecipientActionOutcome::Idempotent
    ));
    assert!(
        matches!(act(&mut recipient.client, &poll, true).await, RecipientActionOutcome::Refused { error } if format!("{error:?}").contains("status was already shared for this item"))
    );
    assert_eq!(
        replies(&recipient, &poll).await,
        turns,
        "repeated acts append zero rows"
    );
    assert!(matches!(
        act(&mut recipient.client, &plain, false).await,
        RecipientActionOutcome::Applied { .. }
    ));
    let mut payloads = Vec::new();
    for item in [&poll, &plain] {
        let wire = recipient
            .sender
            .rpc(
                "x-rustain-items/thread",
                json!({"itemId": item.item().as_str()}),
            )
            .await;
        assert!(wire["error"].is_null(), "{wire}");
        let status = wire["result"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|turn| turn["kind"] == "reply" && turn["replyKind"] == "status")
            .collect::<Vec<_>>();
        assert_eq!(status.len(), 1, "{wire}");
        payloads.push((status[0]["status"].clone(), status[0]["content"].clone()));
    }
    assert_eq!(
        payloads[0], payloads[1],
        "requester text cannot select fields"
    );
    assert_eq!(
        payloads[0].0, preview,
        "team norm cannot broaden member consent"
    );
    let next = request(
        &mut recipient,
        "status-new",
        "status please",
        "status_request",
        PeerRowStage::AwaitingOperator,
    )
    .await;
    let preview = recipient
        .view
        .peer_row_controls
        .values()
        .find(|control| control.item_address.as_ref() == Some(&next))
        .unwrap()
        .status
        .as_ref()
        .unwrap()
        .statement
        .to_wire();
    assert_eq!(
        preview,
        json!({"level":"minimal","stories":[{"key":"S7-03"},{"key":"S7-05"}]})
    );
    assert!(recipient.daemon.requests.lock().is_empty());
    recipient.stop().await;
}
