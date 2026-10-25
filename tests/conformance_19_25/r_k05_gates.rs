//! Story 19.25: every refused retrospective act names its gate and writes zero rows.
use super::attached_harness::{AttachedClient, AttachedDaemon};
use super::d_k04_render::StoryRecipient;
use super::r_harness::act;
use super::s_harness::{replies, request};
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, ProtocolError, RecipientActionOutcome,
};
use rustain::domain::models::{ItemAddress, PeerRowStage, ResponseMode, RetroField};
use serde_json::json;

/// One real attached act that must be refused with exactly `expected` while
/// the recipient journal stays byte-for-byte unchanged.
async fn refused(
    daemon: &AttachedDaemon,
    client: &mut AttachedClient,
    item: &ItemAddress,
    leave_out: Vec<RetroField>,
    decline: bool,
    expected: ProtocolError,
) {
    let before = daemon.journal.entries().await;
    let outcome = act(client, item, leave_out, decline).await;
    assert_eq!(outcome, RecipientActionOutcome::Refused { error: expected });
    assert_eq!(
        daemon.journal.entries().await,
        before,
        "a refused retrospective act writes zero journal rows"
    );
}

fn malformed(text: &str) -> ProtocolError {
    ProtocolError::Malformed(text.to_owned())
}

fn retro_turns(turns: &[serde_json::Value]) -> usize {
    turns
        .iter()
        .filter(|turn| turn["kind"] == json!({"type": "retro"}) || !turn["retro"].is_null())
        .count()
}

#[tokio::test]
async fn r_k05_retro_share_rechecks_writer_tier_and_decline_fields_before_one_share() {
    let mut recipient =
        StoryRecipient::start_for_type(ResponseMode::NotifyAndWait, "retro_request", None).await;
    let item = request(
        &mut recipient,
        "retro-gates",
        "Sprint retro",
        "retro_request",
        PeerRowStage::AwaitingOperator,
    )
    .await;
    assert!(
        recipient
            .view
            .peer_row_controls
            .values()
            .any(|control| control.item_address.as_ref() == Some(&item) && control.retro.is_some()),
        "positive control: the wait card holds the arrival preview"
    );

    // Read-only: a second trusted-local attach while the writer is held.
    let mut observer =
        AttachedClient::attach(&recipient.daemon.path, ConnectionTier::TrustedLocal, true).await;
    assert_eq!(
        observer.granted,
        AttachMode::ReadOnly,
        "writer slot is held"
    );
    refused(
        &recipient.daemon,
        &mut observer,
        &item,
        vec![],
        false,
        ProtocolError::ReadOnly,
    )
    .await;
    observer.detach().await;

    let mut peer = AttachedClient::attach(&recipient.daemon.path, ConnectionTier::Peer, true).await;
    refused(
        &recipient.daemon,
        &mut peer,
        &item,
        vec![],
        false,
        ProtocolError::PeerVerification("recipient reply is same-host trusted-local only".into()),
    )
    .await;
    peer.detach().await;

    refused(
        &recipient.daemon,
        &mut recipient.client,
        &item,
        vec![RetroField::ReplyTimes],
        true,
        malformed("decline takes no field list"),
    )
    .await;
    assert!(
        replies(&recipient, &item).await.is_empty(),
        "no refusal wrote a turn"
    );

    // Positive control: the same trusted writer, a valid Share, one Retro turn.
    let outcome = act(&mut recipient.client, &item, vec![], false).await;
    assert!(
        matches!(outcome, RecipientActionOutcome::Applied { .. }),
        "trusted writer Share must fire: {outcome:?}"
    );
    let turns = replies(&recipient, &item).await;
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert_eq!(retro_turns(&turns), 1, "{turns:?}");
    assert_eq!(turns[0]["kind"], json!({"type": "retro"}), "{turns:?}");
    assert!(turns[0]["retro"].is_object(), "{turns:?}");
    assert!(
        recipient.daemon.requests.lock().is_empty(),
        "human gates never reach a provider"
    );
    recipient.stop().await;
}

#[tokio::test]
async fn r_k05_removed_retro_item_refuses_share_and_decline() {
    let mut recipient =
        StoryRecipient::start_for_type(ResponseMode::NotifyAndWait, "retro_request", None).await;
    let kept = request(
        &mut recipient,
        "kept",
        "Sprint retro",
        "retro_request",
        PeerRowStage::AwaitingOperator,
    )
    .await;
    // Positive control first: this host's writer can decide a retro item.
    let outcome = act(&mut recipient.client, &kept, vec![], true).await;
    assert!(
        matches!(outcome, RecipientActionOutcome::Applied { .. }),
        "{outcome:?}"
    );
    let turns = replies(&recipient, &kept).await;
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert_eq!(retro_turns(&turns), 1, "{turns:?}");
    assert_eq!(
        turns[0]["retro"],
        json!({}),
        "Decline shares exactly nothing()"
    );
    let removed = request(
        &mut recipient,
        "removed",
        "Sprint retro",
        "retro_request",
        PeerRowStage::AwaitingOperator,
    )
    .await;
    recipient
        .client
        .send_frame(ClientFrame::RemoveRecipientItem {
            item_id: removed.item().as_str().to_owned(),
        })
        .await;
    // Serial fence: the daemon answers this only after the removal frame.
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
    // The removal gate precedes every later gate, including decline fields.
    for (leave_out, decline) in [
        (vec![], false),
        (vec![], true),
        (vec![RetroField::AckTimes], true),
    ] {
        refused(
            &recipient.daemon,
            &mut recipient.client,
            &removed,
            leave_out,
            decline,
            malformed("recipient item already removed"),
        )
        .await;
    }
    assert!(replies(&recipient, &removed).await.is_empty());
    assert!(
        recipient.daemon.requests.lock().is_empty(),
        "human gates never reach a provider"
    );
    recipient.stop().await;
}

#[tokio::test]
async fn r_k05_wrong_type_and_automatic_retro_items_have_nothing_to_share() {
    for (message_type, mode, stage, refusal) in [
        (
            "consultation",
            ResponseMode::NotifyAndWait,
            PeerRowStage::AwaitingOperator,
            "retrospective replies answer retro_request items only",
        ),
        (
            "retro_request",
            ResponseMode::NotifyAndAuto,
            PeerRowStage::Settled,
            "no retrospective summary was prepared for this item",
        ),
    ] {
        let mut recipient = StoryRecipient::start_for_type(mode, message_type, None).await;
        let item = request(&mut recipient, "ungated", "private", message_type, stage).await;
        assert!(
            recipient
                .view
                .peer_row_controls
                .values()
                .any(|control| control.item_address.as_ref() == Some(&item)
                    && control.stage == stage),
            "positive control: the real front door reached {stage:?} for {message_type}"
        );
        assert!(
            recipient
                .view
                .peer_row_controls
                .values()
                .filter(|control| control.item_address.as_ref() == Some(&item))
                .all(|control| control.retro.is_none()),
            "{message_type} under {mode:?} carries no preview"
        );
        // The automatic template path, if it answered, contributed nothing.
        assert_eq!(retro_turns(&replies(&recipient, &item).await), 0);
        // Type and preparation gates precede the decline-fields gate.
        for (leave_out, decline) in [
            (vec![], false),
            (vec![], true),
            (vec![RetroField::Received], true),
        ] {
            refused(
                &recipient.daemon,
                &mut recipient.client,
                &item,
                leave_out,
                decline,
                malformed(refusal),
            )
            .await;
        }
        assert_eq!(
            retro_turns(&replies(&recipient, &item).await),
            0,
            "the gate produces no Retro turn"
        );
        recipient.stop().await;
    }
}
