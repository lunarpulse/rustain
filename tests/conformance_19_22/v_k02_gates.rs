//! Story 19.22 K02: verdict gates. Discuss is allowed wherever a verdict may be
//! offered; Accept only on items that carry `you`; a verdict never lands on a
//! non-list or removed item; an empty Discuss and lesser attachments are
//! refused — every refusal writes zero rows.
//!
use std::sync::Arc;

use super::attached_harness::{AttachedClient, AttachedDaemon, RecipientHost};
use super::harness::read_room_events;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, ProtocolError, RecipientAction,
    RecipientActionOutcome, RecipientActionTarget, ReplyVerdict, TeamSendOutcome,
};
use rustain::domain::models::{
    IndividualPolicy, ReplyKind, RoomEvent, SemanticMessageType, StoryManifest,
};

fn notify_and_wait_policy() -> Arc<rustain::domain::models::EffectivePolicy> {
    Arc::new(
        rustain::domain::services::team_policy::resolve_effective_policy(
            &IndividualPolicy::default(),
            None,
            &[],
        ),
    )
}

async fn reply_rows(journal: &super::harness::Journal) -> Vec<(String, ReplyKind)> {
    read_room_events(journal)
        .await
        .into_iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::RecipientItemReplied { content, kind, .. } => Some((content, kind)),
            _ => None,
        })
        .collect()
}

async fn expect_refusal(client: &mut AttachedClient, item_id: &str) -> ProtocolError {
    let frame = client
        .wait_for(|frame| {
            matches!(
                frame,
                DaemonFrame::RecipientActionResult {
                    action: RecipientAction::Reply,
                    target: RecipientActionTarget::Item { item_id: answered },
                    ..
                } if answered == item_id
            )
        })
        .await;
    let DaemonFrame::RecipientActionResult {
        outcome: RecipientActionOutcome::Refused { error },
        ..
    } = frame
    else {
        panic!("expected a refusal, got {frame:?}")
    };
    error
}

#[tokio::test]
async fn v_k02_verdict_gates_refuse_without_rows() {
    let mut b = RecipientHost::start_with_effective_policy(
        A2aAdmissionPolicy::Allow,
        notify_and_wait_policy(),
    )
    .await;
    let socket = b.enable_operator().await;
    let mut operator = AttachedClient::attach(&socket, ConnectionTier::TrustedLocal, false).await;
    let daemon = AttachedDaemon::start(vec![b.peer("B")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    // A scope_change that does not carry `you`: DiscussOnly.
    writer
        .send_team(
            "scope-other",
            vec!["B".into()],
            "S7-07 moves to the next sprint",
            Some(SemanticMessageType::ScopeChange),
            None,
            Some(StoryManifest::parse("S7-07").unwrap()),
            None,
        )
        .await;
    assert!(matches!(
        writer.wait_send_settled("scope-other", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    let scope_item = read_room_events(&b.journal)
        .await
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address,
                content: body,
                ..
            } if body == "S7-07 moves to the next sprint" => {
                Some(address.item().as_str().to_owned())
            }
            _ => None,
        })
        .expect("scope_change ingress");

    // Discuss on it is one typed turn the asker reads back.
    operator
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: scope_item.clone(),
            text: "concern: the release date slips".into(),
            verdict: Some(ReplyVerdict::Discuss),
        })
        .await;
    operator
        .wait_for(|frame| {
            matches!(
                frame,
                DaemonFrame::RecipientActionResult {
                    action: RecipientAction::Reply,
                    target: RecipientActionTarget::Item { item_id: answered },
                    outcome: RecipientActionOutcome::Applied { .. },
                } if answered == &scope_item
            )
        })
        .await;
    let rows = reply_rows(&b.journal).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, ReplyKind::Discuss);
    writer
        .send_frame(ClientFrame::TeamThread {
            request: "thread-discuss".into(),
            peer: "B".into(),
            item_id: scope_item.clone(),
        })
        .await;
    let thread = writer
        .wait_board_result("thread-discuss")
        .await
        .expect("thread view");
    assert!(thread.contains(" [verdict: discuss concerns]"), "{thread}");

    // Accept on a DiscussOnly item is refused, zero rows.
    let before = reply_rows(&b.journal).await.len();
    let before_events = read_room_events(&b.journal).await.len();
    operator
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: scope_item.clone(),
            text: String::new(),
            verdict: Some(ReplyVerdict::Accept),
        })
        .await;
    assert_eq!(
        expect_refusal(&mut operator, &scope_item).await,
        ProtocolError::Malformed("accept answers an item that assigns you a story".into())
    );
    assert_eq!(reply_rows(&b.journal).await.len(), before);
    assert_eq!(
        read_room_events(&b.journal).await.len(),
        before_events,
        "Accept refusal writes zero rows"
    );

    // A non-list item carries no verdict at all.
    writer
        .send_team(
            "plain-design",
            vec!["B".into()],
            "design note",
            Some(SemanticMessageType::DesignUpdate),
            None,
            None,
            None,
        )
        .await;
    assert!(matches!(
        writer.wait_send_settled("plain-design", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    let design_item = read_room_events(&b.journal)
        .await
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address,
                content: body,
                ..
            } if body == "design note" => Some(address.item().as_str().to_owned()),
            _ => None,
        })
        .expect("design_update ingress");
    let before_events = read_room_events(&b.journal).await.len();
    operator
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: design_item.clone(),
            text: "noted".into(),
            verdict: Some(ReplyVerdict::Discuss),
        })
        .await;
    assert_eq!(
        expect_refusal(&mut operator, &design_item).await,
        ProtocolError::Malformed("a verdict answers an item that carries a story list".into())
    );
    assert_eq!(reply_rows(&b.journal).await.len(), before);
    assert_eq!(
        read_room_events(&b.journal).await.len(),
        before_events,
        "non-list refusal writes zero rows"
    );

    // A removed item refuses the verdict like any recipient act.
    operator
        .send_frame(ClientFrame::RemoveRecipientItem {
            item_id: design_item.clone(),
        })
        .await;
    // Remove has no success frame; a FIFO history request fences its durable
    // append before the following verdict, as in the 19.24 gate conformance.
    operator
        .send_frame(ClientFrame::HistoryRequest {
            before_index: Some(0),
            count: 1,
        })
        .await;
    operator
        .wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
        .await;
    let before_events = read_room_events(&b.journal).await.len();
    operator
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: design_item.clone(),
            text: "too late".into(),
            verdict: Some(ReplyVerdict::Discuss),
        })
        .await;
    assert_eq!(
        expect_refusal(&mut operator, &design_item).await,
        ProtocolError::Malformed("recipient item already removed".into())
    );
    assert_eq!(reply_rows(&b.journal).await.len(), before);
    assert_eq!(
        read_room_events(&b.journal).await.len(),
        before_events,
        "removed-item refusal writes zero rows"
    );

    // Discuss requires text.
    let before_events = read_room_events(&b.journal).await.len();
    operator
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: scope_item.clone(),
            text: String::new(),
            verdict: Some(ReplyVerdict::Discuss),
        })
        .await;
    assert_eq!(
        expect_refusal(&mut operator, &scope_item).await,
        ProtocolError::Malformed("reply text is empty".into())
    );
    assert_eq!(reply_rows(&b.journal).await.len(), before);
    assert_eq!(
        read_room_events(&b.journal).await.len(),
        before_events,
        "empty Discuss writes zero rows"
    );

    // A read-only attachment cannot give a verdict.
    let mut read_only = AttachedClient::attach(&socket, ConnectionTier::TrustedLocal, true).await;
    let before_events = read_room_events(&b.journal).await.len();
    read_only
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: scope_item.clone(),
            text: "read-only verdict".into(),
            verdict: Some(ReplyVerdict::Discuss),
        })
        .await;
    assert_eq!(
        expect_refusal(&mut read_only, &scope_item).await,
        ProtocolError::ReadOnly
    );
    assert_eq!(
        read_room_events(&b.journal).await.len(),
        before_events,
        "read-only refusal writes zero rows"
    );

    // A Peer-tier attachment is same-host trusted-local only.
    let mut peer_tier = AttachedClient::attach(&socket, ConnectionTier::Peer, true).await;
    let before_events = read_room_events(&b.journal).await.len();
    peer_tier
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: scope_item.clone(),
            text: "peer verdict".into(),
            verdict: Some(ReplyVerdict::Discuss),
        })
        .await;
    assert!(matches!(
        expect_refusal(&mut peer_tier, &scope_item).await,
        ProtocolError::PeerVerification(_)
    ));
    assert_eq!(reply_rows(&b.journal).await.len(), before);
    assert_eq!(
        read_room_events(&b.journal).await.len(),
        before_events,
        "Peer-tier refusal writes zero rows"
    );

    read_only.detach().await;
    peer_tier.detach().await;
    writer.detach().await;
    operator.detach().await;
    daemon.stop().await;
    b.stop().await;
}
