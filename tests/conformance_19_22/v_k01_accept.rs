//! Story 19.22 K01: an Accept verdict is one journaled reply turn — never an
//! acknowledgement, never item state — entering through the real attached
//! `ReplyToRecipientItem` frame on the recipient's operator socket.
//!
use std::sync::Arc;

use super::attached_harness::{AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT, RecipientHost};
use super::d_k04_render::snapshot_view;
use super::harness::read_room_events;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionTarget, ReplyVerdict,
    TeamSendOutcome,
};
use rustain::domain::models::{
    Conversation, PeerRowStage, ReplyKind, RoomEvent, SemanticMessageType, StoryManifest,
};
use rustain::infrastructure::runtime::attach_loop::apply_peer_row_operation;

fn notify_and_wait_policy() -> Arc<rustain::domain::models::EffectivePolicy> {
    Arc::new(
        rustain::domain::services::team_policy::resolve_effective_policy(
            &rustain::domain::models::IndividualPolicy::default(),
            None,
            &[],
        ),
    )
}

#[tokio::test]
async fn v_k01_accept_verdict_is_one_reply_turn() {
    // Notify-and-wait recipient B: the story card must be live when answered.
    let mut b = RecipientHost::start_with_effective_policy(
        A2aAdmissionPolicy::Allow,
        notify_and_wait_policy(),
    )
    .await;
    let socket = b.enable_operator().await;
    let mut operator = AttachedClient::attach(&socket, ConnectionTier::TrustedLocal, false).await;
    let mut view: Conversation = snapshot_view(&operator);
    let daemon = AttachedDaemon::start(vec![b.peer("B")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    writer
        .send_team(
            "story-for-b",
            vec!["B".into()],
            "S7-09: Fix payment timeout",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(StoryManifest::parse("S7-09=B").unwrap()),
            None,
        )
        .await;
    assert!(matches!(
        writer.wait_send_settled("story-for-b", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    let address = read_room_events(&b.journal)
        .await
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address,
                content: body,
                ..
            } if body == "S7-09: Fix payment timeout" => Some(address),
            _ => None,
        })
        .expect("real recipient ingress");
    let item_id = address.item().as_str().to_owned();

    // Track the live card projection until the story card is pending…
    let mut cursor = 0usize;
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            for frame in &operator.received[cursor..] {
                if let DaemonFrame::PeerRowOperation(operation) = frame {
                    apply_peer_row_operation(&mut view, operation.as_ref().clone());
                }
            }
            cursor = operator.received.len();
            if view.peer_row_controls.values().any(|control| {
                control.item_address.as_ref() == Some(&address)
                    && control.stage == PeerRowStage::AwaitingOperator
            }) {
                return;
            }
            operator.collect_one().await;
        }
    })
    .await
    .expect("live story card reaches the operator");

    // …then answer it with the `[y]`-equivalent frame.
    operator
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: item_id.clone(),
            text: String::new(),
            verdict: Some(ReplyVerdict::Accept),
        })
        .await;
    let receipt = operator
        .wait_for(|frame| {
            matches!(
                frame,
                DaemonFrame::RecipientActionResult {
                    action: RecipientAction::Reply,
                    target: RecipientActionTarget::Item { item_id: answered },
                    ..
                } if answered == &item_id
            )
        })
        .await;
    let DaemonFrame::RecipientActionResult {
        action: RecipientAction::Reply,
        target: RecipientActionTarget::Item { item_id: answered },
        outcome: rustain::adapters::daemon::protocol::RecipientActionOutcome::Applied { .. },
    } = receipt
    else {
        panic!("an accept on a live card is applied, got {receipt:?}")
    };
    assert_eq!(answered, item_id);

    // Exactly one Accept turn, empty accept text normalized, no ack, no state change.
    let replies: Vec<_> = read_room_events(&b.journal)
        .await
        .into_iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::RecipientItemReplied {
                content,
                kind,
                via_node,
                authorship,
                send_authorization,
                ..
            } => Some((content, kind, via_node, authorship, send_authorization)),
            _ => None,
        })
        .collect();
    assert_eq!(replies.len(), 1, "exactly one verdict turn: {replies:?}");
    assert_eq!(replies[0].0, "Accepted.");
    assert_eq!(replies[0].1, ReplyKind::Accept);
    assert!(
        replies[0].2.is_some(),
        "a live-card verdict is via the node"
    );
    assert_eq!(
        replies[0].3,
        rustain::domain::models::Authorship::HumanWritten
    );
    assert_eq!(
        replies[0].4,
        rustain::domain::models::SendAuthorization::HumanDecided
    );
    assert!(
        read_room_events(&b.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemAcknowledged { .. })),
        "a verdict is a reply, never an acknowledgement"
    );
    let entries = b.journal.entries().await;
    let item = rustain::adapters::policy::JournalRecipientItemProjection::from_entries(&entries)
        .snapshot()
        .items
        .get(&address)
        .expect("item survives the verdict")
        .clone();
    assert!(
        matches!(
            item.state,
            rustain::domain::models::RecipientItemState::Received { .. }
        ),
        "a verdict never changes item state"
    );

    // The pending card settles.
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            for frame in &operator.received[cursor..] {
                if let DaemonFrame::PeerRowOperation(operation) = frame {
                    apply_peer_row_operation(&mut view, operation.as_ref().clone());
                }
            }
            cursor = operator.received.len();
            if view.peer_row_controls.values().any(|control| {
                control.item_address.as_ref() == Some(&address)
                    && control.stage == PeerRowStage::Settled
            }) {
                return;
            }
            operator.collect_one().await;
        }
    })
    .await
    .expect("answered card settles");

    // The asker's served thread shows the verdict header.
    writer
        .send_frame(ClientFrame::TeamThread {
            request: "thread-accept".into(),
            peer: "B".into(),
            item_id: item_id.clone(),
        })
        .await;
    let thread = writer
        .wait_board_result("thread-accept")
        .await
        .expect("thread view");
    assert!(thread.contains(" [verdict: accept]"), "{thread}");

    // Settled items remain answerable: a later verdict is a new direct turn,
    // not an idempotent card decision or an implicit acknowledgement.
    operator
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: item_id.clone(),
            text: "Accepted with context.".into(),
            verdict: Some(ReplyVerdict::Accept),
        })
        .await;
    operator
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::RecipientActionResult {
        action: RecipientAction::Reply,
        target: RecipientActionTarget::Item { item_id: answered },
        outcome: rustain::adapters::daemon::protocol::RecipientActionOutcome::Applied { .. },
    } if answered == &item_id)
        })
        .await;
    let events = read_room_events(&b.journal).await;
    assert_eq!(
        events
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemReplied { .. }))
            .count(),
        2
    );
    assert!(events.iter().any(|(_, event)| matches!(event,
        RoomEvent::RecipientItemReplied { content, kind: ReplyKind::Accept, via_node: None, .. }
        if content == "Accepted with context."
    )));

    // Control: a plain reply (no verdict) on a sibling item stays an Answer.
    writer
        .send_team(
            "story-sibling",
            vec!["B".into()],
            "S7-10: another story",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(StoryManifest::parse("S7-10=B").unwrap()),
            None,
        )
        .await;
    assert!(matches!(
        writer.wait_send_settled("story-sibling", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    let sibling = read_room_events(&b.journal)
        .await
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address,
                content: body,
                ..
            } if body == "S7-10: another story" => Some(address.item().as_str().to_owned()),
            _ => None,
        })
        .expect("sibling ingress");
    operator
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: sibling.clone(),
            text: "plain answer".into(),
            verdict: None,
        })
        .await;
    operator
        .wait_for(|frame| {
            matches!(
                frame,
                DaemonFrame::RecipientActionResult {
                    action: RecipientAction::Reply,
                    target: RecipientActionTarget::Item { item_id: answered },
                    ..
                } if answered == &sibling
            )
        })
        .await;
    let answers: Vec<_> = read_room_events(&b.journal)
        .await
        .into_iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::RecipientItemReplied { content, kind, .. } => Some((content, kind)),
            _ => None,
        })
        .collect();
    assert_eq!(answers.len(), 3);
    assert!(
        answers
            .iter()
            .any(|(content, kind)| content == "plain answer" && *kind == ReplyKind::Answer)
    );

    // A verdict is human-decided: no provider was ever consulted anywhere.
    assert!(b.requests.lock().is_empty());
    assert_eq!(daemon.provider_request_count(), 0);
    assert_eq!(daemon.model_turn_count(), 0);

    // The sibling's thread carries no verdict marker.
    writer
        .send_frame(ClientFrame::TeamThread {
            request: "thread-sibling".into(),
            peer: "B".into(),
            item_id: sibling,
        })
        .await;
    let sibling_thread = writer
        .wait_board_result("thread-sibling")
        .await
        .expect("sibling thread");
    assert!(!sibling_thread.contains("[verdict:"), "{sibling_thread}");

    writer.detach().await;
    operator.detach().await;
    daemon.stop().await;
    b.stop().await;
}
