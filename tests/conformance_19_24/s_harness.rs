//! Real HTTP/attach helpers; no fabricated status statements or reply turns.
use super::attached_harness::AttachedClient;
use super::d_k04_render::StoryRecipient;
use rustain::adapters::daemon::protocol::{
    ClientFrame, DaemonFrame, RecipientAction, RecipientActionOutcome, RecipientActionTarget,
};
use rustain::domain::models::{ItemAddress, JournalRecord, PeerRowStage, RoomEvent};
use serde_json::json;

pub async fn request(
    recipient: &mut StoryRecipient,
    task: &str,
    body: &str,
    message_type: &str,
    stage: PeerRowStage,
) -> ItemAddress {
    let result = recipient
        .sender
        .message_send_with_metadata(task, body, json!({"x-rustain-message-type": message_type}))
        .await;
    assert!(result.json["error"].is_null(), "{result:?}");
    recipient.daemon.wait_for_received_task(task).await;
    let address = recipient
        .daemon
        .journal
        .entries()
        .await
        .into_iter()
        .find_map(|entry| match entry.record {
            JournalRecord::Room(RoomEvent::RecipientItemReceived {
                address,
                task: received,
                ..
            }) if received == task => Some(address),
            _ => None,
        })
        .expect("HTTP front door journals the item");
    recipient.wait_control(&address, stage).await;
    address
}

pub async fn act(
    client: &mut AttachedClient,
    address: &ItemAddress,
    decline: bool,
) -> RecipientActionOutcome {
    let id = address.item().as_str();
    client
        .send_frame(ClientFrame::ShareRecipientStatus {
            item_id: id.to_owned(),
            decline,
        })
        .await;
    match client
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::RecipientActionResult {
        action: RecipientAction::ShareStatus, target: RecipientActionTarget::Item { item_id }, ..
    } if item_id == id)
        })
        .await
    {
        DaemonFrame::RecipientActionResult { outcome, .. } => outcome,
        other => panic!("correlated status action result expected: {other:?}"),
    }
}

pub async fn replies(recipient: &StoryRecipient, address: &ItemAddress) -> Vec<serde_json::Value> {
    recipient
        .daemon
        .journal
        .entries()
        .await
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(event @ RoomEvent::RecipientItemReplied { .. }) => {
                let value = serde_json::to_value(event).unwrap();
                (value["answers"] == address.item().as_str()).then_some(value)
            }
            _ => None,
        })
        .collect()
}

pub fn status_policy(level: &str) -> std::sync::Arc<rustain::domain::models::EffectivePolicy> {
    use rustain::domain::models::{
        IndividualDefaults, IndividualPolicy, NotificationUrgency, ResponseMode, TeamPolicy,
        TeamTypeOverride,
    };
    let individual = IndividualPolicy {
        defaults: IndividualDefaults {
            response_mode: Some(ResponseMode::NotifyAndAuto),
            notification: Some(NotificationUrgency::Immediate),
            status_detail_minimum: Some(level.to_owned()),
            ..IndividualDefaults::default()
        },
        ..IndividualPolicy::default()
    };
    let mut team = TeamPolicy::default();
    team.overrides.per_type.insert(
        "status_request".into(),
        toml::Value::try_from(TeamTypeOverride {
            response_mode: Some(ResponseMode::NotifyAndWait),
            notification: None,
        })
        .unwrap(),
    );
    std::sync::Arc::new(
        rustain::domain::services::team_policy::resolve_effective_policy(
            &individual,
            Some(&team),
            &[],
        ),
    )
}

pub async fn host_item(
    host: &super::attached_harness::RecipientHost,
    message_type: rustain::domain::models::SemanticMessageType,
) -> ItemAddress {
    host.journal
        .entries()
        .await
        .into_iter()
        .rev()
        .find_map(|entry| match entry.record {
            JournalRecord::Room(RoomEvent::RecipientItemReceived {
                address,
                message_type: received,
                ..
            }) if received == message_type => Some(address),
            _ => None,
        })
        .expect("real send delivered an item of this type")
}

pub async fn wait_host_control(
    client: &mut AttachedClient,
    address: &ItemAddress,
) -> rustain::domain::models::PeerRowControl {
    use rustain::infrastructure::runtime::attach_loop::apply_peer_row_operation;
    tokio::time::timeout(super::attached_harness::FIXTURE_TIMEOUT, async {
        let mut view = super::d_k04_render::snapshot_view(client);
        let mut cursor = 0;
        loop {
            for frame in &client.received[cursor..] {
                if let DaemonFrame::PeerRowOperation(operation) = frame {
                    apply_peer_row_operation(&mut view, operation.as_ref().clone());
                }
            }
            cursor = client.received.len();
            if let Some(control) = view.peer_row_controls.values().find(|control| {
                control.item_address.as_ref() == Some(address)
                    && control.stage == PeerRowStage::AwaitingOperator
            }) {
                return control.clone();
            }
            client.collect_one().await;
        }
    })
    .await
    .expect("prepared live status card deadline")
}
