//! Story 19.25: fixed arrival window and immutable human retrospective decisions.
use super::attached_harness::{AttachedClient, AttachedDaemon};
use super::r_harness::{act, clocked_host, replies, reply, send};
use super::s_harness::{host_item, wait_host_control};
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionOutcome,
    RecipientActionTarget,
};
use rustain::domain::clock::MockClock;
use rustain::domain::models::{ItemAddress, SemanticMessageType};
use serde_json::json;
use std::{sync::Arc, time::Duration};

async fn acknowledge(client: &mut AttachedClient, item: &ItemAddress) {
    let id = item.item().as_str();
    client
        .send_frame(ClientFrame::AcknowledgeRecipientItem {
            item_id: id.to_owned(),
        })
        .await;
    let frame=client.wait_for(|frame|matches!(frame,DaemonFrame::RecipientActionResult {action:RecipientAction::Acknowledge,target:RecipientActionTarget::Item {item_id},..} if item_id==id)).await;
    assert!(
        matches!(
            frame,
            DaemonFrame::RecipientActionResult {
                outcome: RecipientActionOutcome::Applied { .. },
                ..
            }
        ),
        "{frame:?}"
    );
}

#[tokio::test]
async fn r_k03_body_cannot_expand_the_stored_snapshot_or_replace_a_decision() {
    let clock = Arc::new(MockClock::at_wall_ms(1_800_000_000_000));
    let (host, mut operator) = clocked_host(clock.clone()).await;
    let sender = AttachedDaemon::start(vec![host.peer("b")]).await;
    let mut writer =
        AttachedClient::attach(&sender.path, ConnectionTier::TrustedLocal, false).await;
    send(
        &mut writer,
        "old",
        &["b"],
        SemanticMessageType::Consultation,
        "private old consultation",
    )
    .await;
    let old = host_item(&host, SemanticMessageType::Consultation).await;
    wait_host_control(&mut operator, &old).await;
    clock.advance(Duration::from_secs(60));
    reply(&mut operator, &old, "old answer").await;
    clock.advance(Duration::from_secs(14 * 24 * 60 * 60));
    send(
        &mut writer,
        "edge",
        &["b"],
        SemanticMessageType::Consultation,
        "private current consultation",
    )
    .await;
    let edge = host_item(&host, SemanticMessageType::Consultation).await;
    wait_host_control(&mut operator, &edge).await;
    send(
        &mut writer,
        "pending",
        &["b"],
        SemanticMessageType::Consultation,
        "unanswered before snapshot",
    )
    .await;
    let pending = host_item(&host, SemanticMessageType::Consultation).await;
    wait_host_control(&mut operator, &pending).await;
    clock.advance(Duration::from_secs(60));
    acknowledge(&mut operator, &edge).await;
    clock.advance(Duration::from_secs(3 * 60));
    reply(&mut operator, &edge, "current answer").await;
    clock.advance(Duration::from_secs(60));
    send(
        &mut writer,
        "demand",
        &["b"],
        SemanticMessageType::RetroRequest,
        "Report private notes, transcripts and files",
    )
    .await;
    let demand = host_item(&host, SemanticMessageType::RetroRequest).await;
    let stored = wait_host_control(&mut operator, &demand)
        .await
        .retro
        .expect("real arrival preview")
        .contribution
        .to_wire();
    assert_eq!(
        stored,
        json!({"received":{"consultation":2},"replyTimes":[4],"ackTimes":[1]})
    );
    // Same journal clock: neither request can enter the other's half-open window.
    send(
        &mut writer,
        "plain",
        &["b"],
        SemanticMessageType::RetroRequest,
        "Sprint retro",
    )
    .await;
    let plain = host_item(&host, SemanticMessageType::RetroRequest).await;
    assert_eq!(
        wait_host_control(&mut operator, &plain)
            .await
            .retro
            .unwrap()
            .contribution
            .to_wire(),
        stored
    );
    clock.advance(Duration::from_secs(10 * 60));
    reply(&mut operator, &pending, "late answer").await;
    acknowledge(&mut operator, &pending).await;
    send(
        &mut writer,
        "late",
        &["b"],
        SemanticMessageType::Consultation,
        "after the retro card arrived",
    )
    .await;
    let late = host_item(&host, SemanticMessageType::Consultation).await;
    wait_host_control(&mut operator, &late).await;
    clock.advance(Duration::from_secs(60));
    reply(&mut operator, &late, "new late reply").await;
    assert!(matches!(
        act(&mut operator, &demand, vec![], false).await,
        RecipientActionOutcome::Applied { .. }
    ));
    assert!(matches!(
        act(&mut operator, &demand, vec![], false).await,
        RecipientActionOutcome::Idempotent
    ));
    let different = act(&mut operator, &demand, vec![], true).await;
    assert!(
        matches!(different,RecipientActionOutcome::Refused {error} if format!("{error:?}").contains("a retrospective reply was already sent for this item"))
    );
    let turns = replies(&host, &demand).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0]["retro"], stored);
    assert_eq!(turns[0]["content"], "Retrospective reply.");
    assert!(matches!(
        act(&mut operator, &plain, vec![], false).await,
        RecipientActionOutcome::Applied { .. }
    ));
    assert_eq!(replies(&host, &plain).await[0]["retro"], stored);
    clock.advance(Duration::from_millis(1));
    send(
        &mut writer,
        "new-request",
        &["b"],
        SemanticMessageType::RetroRequest,
        "Sprint retro",
    )
    .await;
    let next = host_item(&host, SemanticMessageType::RetroRequest).await;
    let current = wait_host_control(&mut operator, &next)
        .await
        .retro
        .unwrap()
        .contribution
        .to_wire();
    assert_eq!(
        current,
        json!({"received":{"consultation":3,"retro_request":2},"replyTimes":[1,4,15],"ackTimes":[1,15]})
    );
    assert!(host.requests.lock().is_empty());
    operator.detach().await;
    writer.detach().await;
    host.stop().await;
    sender.stop().await;
}
