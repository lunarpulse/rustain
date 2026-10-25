//! Story 19.24: every rejected status act leaves the journal unchanged.
use super::attached_harness::AttachedClient;
use super::d_k04_render::StoryRecipient;
use super::s_harness::{act, request};
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, RecipientActionOutcome,
};
use rustain::domain::models::{PeerRowStage, ResponseMode};

#[tokio::test]
async fn s_k05_status_acts_recheck_writer_tier_type_preparation_and_removal() {
    let mut recipient = StoryRecipient::start_status(ResponseMode::NotifyAndWait, "minimal").await;
    let item = request(
        &mut recipient,
        "authority",
        "status?",
        "status_request",
        PeerRowStage::AwaitingOperator,
    )
    .await;
    for tier in [ConnectionTier::TrustedLocal, ConnectionTier::Peer] {
        let mut client = AttachedClient::attach(&recipient.daemon.path, tier, true).await;
        let before = recipient.daemon.journal.entries().await;
        let outcome = act(&mut client, &item, false).await;
        assert!(
            matches!(outcome, RecipientActionOutcome::Refused { .. }),
            "{outcome:?}"
        );
        let text = format!("{outcome:?}");
        assert!(
            if tier == ConnectionTier::TrustedLocal {
                text.contains("ReadOnly")
            } else {
                text.contains("trusted-local only")
            },
            "{text}"
        );
        assert_eq!(
            recipient.daemon.journal.entries().await,
            before,
            "authority refusal writes zero rows"
        );
        client.detach().await;
    }
    assert!(
        matches!(
            act(&mut recipient.client, &item, false).await,
            RecipientActionOutcome::Applied { .. }
        ),
        "trusted writer control must fire"
    );
    let removed = request(
        &mut recipient,
        "removed",
        "status?",
        "status_request",
        PeerRowStage::AwaitingOperator,
    )
    .await;
    recipient
        .client
        .send_frame(ClientFrame::RemoveRecipientItem {
            item_id: removed.item().as_str().to_owned(),
        })
        .await;
    recipient
        .client
        .send_frame(ClientFrame::HistoryRequest {
            before_index: Some(0),
            count: 1,
        })
        .await;
    recipient
        .client
        .wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
        .await;
    let before = recipient.daemon.journal.entries().await;
    let outcome = act(&mut recipient.client, &removed, false).await;
    assert!(
        format!("{outcome:?}").contains("recipient item already removed"),
        "{outcome:?}"
    );
    assert_eq!(recipient.daemon.journal.entries().await, before);
    recipient.stop().await;

    for (message_type, mode, stage, refusal) in [
        (
            "consultation",
            ResponseMode::NotifyAndWait,
            PeerRowStage::AwaitingOperator,
            "status sharing answers status_request items only",
        ),
        (
            "status_request",
            ResponseMode::NotifyAndAuto,
            PeerRowStage::Settled,
            "no status statement was prepared for this item",
        ),
    ] {
        let mut recipient =
            StoryRecipient::start_for_type(mode, message_type, Some("status")).await;
        let item = request(
            &mut recipient,
            "wrong-or-unprepared",
            "private",
            message_type,
            stage,
        )
        .await;
        let before = recipient.daemon.journal.entries().await;
        let outcome = act(&mut recipient.client, &item, false).await;
        assert!(format!("{outcome:?}").contains(refusal), "{outcome:?}");
        assert_eq!(recipient.daemon.journal.entries().await, before);
        recipient.stop().await;
    }
}
