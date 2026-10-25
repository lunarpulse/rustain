//! Story 19.25: real attached acts, clocked journal hosts and report reads.
use super::attached_harness::{AttachedClient, RecipientHost};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionOutcome,
    RecipientActionTarget, TeamBoardRequest, TeamSendOutcome,
};
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    EffectivePolicy, IndividualDefaults, IndividualPolicy, ItemAddress, JournalRecord,
    NotificationUrgency, RetroField, RoomEvent, SemanticMessageType,
};
use std::sync::Arc;

pub fn retro_policy() -> Arc<EffectivePolicy> {
    Arc::new(
        rustain::domain::services::team_policy::resolve_effective_policy(
            &IndividualPolicy {
                defaults: IndividualDefaults {
                    response_mode: Some(rustain::domain::models::ResponseMode::NotifyAndWait),
                    notification: Some(NotificationUrgency::Immediate),
                    ..IndividualDefaults::default()
                },
                ..IndividualPolicy::default()
            },
            None,
            &[],
        ),
    )
}

pub async fn clocked_host(clock: Arc<MockClock>) -> (RecipientHost, AttachedClient) {
    let mut host = RecipientHost::start_with_clock_and_policy(
        A2aAdmissionPolicy::Allow,
        clock,
        retro_policy(),
    )
    .await;
    let path = host.enable_operator().await;
    let client = AttachedClient::attach(&path, ConnectionTier::TrustedLocal, false).await;
    (host, client)
}

pub async fn send(
    client: &mut AttachedClient,
    key: &str,
    aliases: &[&str],
    kind: SemanticMessageType,
    body: &str,
) -> Vec<TeamSendOutcome> {
    client
        .send_team(
            key,
            aliases.iter().map(|alias| (*alias).to_owned()).collect(),
            body,
            Some(kind),
            None,
            None,
            None,
        )
        .await;
    let mut outcomes = Vec::new();
    for index in 0..aliases.len() {
        match client.wait_send_settled(key, index).await {
            DaemonFrame::TeamSendSettled { outcome, .. } => outcomes.push(outcome),
            frame => panic!("send must settle through the real daemon: {frame:?}"),
        }
    }
    outcomes
}

pub async fn act(
    client: &mut AttachedClient,
    item: &ItemAddress,
    leave_out: Vec<RetroField>,
    decline: bool,
) -> RecipientActionOutcome {
    let item_id = item.item().as_str();
    client
        .send_frame(ClientFrame::ShareRecipientRetro {
            item_id: item_id.to_owned(),
            leave_out,
            decline,
        })
        .await;
    match client.wait_for(|frame| matches!(frame, DaemonFrame::RecipientActionResult { action: RecipientAction::ShareRetro, target: RecipientActionTarget::Item { item_id: actual }, .. } if actual == item_id)).await {
        DaemonFrame::RecipientActionResult { outcome, .. } => outcome,
        frame => panic!("retrospective act receipt: {frame:?}"),
    }
}

pub async fn reply(client: &mut AttachedClient, item: &ItemAddress, text: &str) {
    let item_id = item.item().as_str();
    client
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: item_id.to_owned(),
            text: text.to_owned(),
            verdict: None,
        })
        .await;
    let frame=client.wait_for(|frame| matches!(frame, DaemonFrame::RecipientActionResult { action: RecipientAction::Reply, target: RecipientActionTarget::Item { item_id: actual }, .. } if actual == item_id)).await;
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

pub async fn replies(host: &RecipientHost, item: &ItemAddress) -> Vec<serde_json::Value> {
    host.journal
        .entries()
        .await
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(event @ RoomEvent::RecipientItemReplied { .. }) => {
                let value = serde_json::to_value(event).unwrap();
                (value["answers"] == item.item().as_str()).then_some(value)
            }
            _ => None,
        })
        .collect()
}

pub async fn board(client: &mut AttachedClient, key: &str) -> String {
    client.team_board(key, TeamBoardRequest::Last).await;
    client
        .wait_board_result(key)
        .await
        .expect("real board read")
}
