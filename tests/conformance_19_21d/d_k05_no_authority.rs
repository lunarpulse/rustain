//! Story 19.21d K05: a sender's story list never grants work or context authority.
use super::attached_harness::{AttachedClient, AttachedDaemon};
use super::d_k04_render::{StoryRecipient, folded_stories, listed_carrier, oldest_pending};
use rustain::adapters::daemon::protocol::{
    ClientFrame, DaemonFrame, RecipientAction, RecipientActionOutcome, RecipientActionTarget,
};
use rustain::domain::models::{PeerRowStage, ReceivedStories, ResponseMode};

async fn inspect_context(client: &mut AttachedClient) -> (Vec<String>, String) {
    let start = client.received.len();
    client
        .send_frame(ClientFrame::InspectRecipientContext)
        .await;
    // Context changes also publish state. The FIFO history reply fences both
    // those notifications and this explicit read, so use the latest state.
    client
        .send_frame(ClientFrame::HistoryRequest {
            before_index: Some(0),
            count: 1,
        })
        .await;
    client
        .wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
        .await;
    client.received[start..]
        .iter()
        .rev()
        .find_map(|frame| match frame {
            DaemonFrame::RecipientContextState {
                selected_item_ids,
                inventory,
                ..
            } => Some((selected_item_ids.clone(), inventory.clone())),
            _ => None,
        })
        .expect("explicit context read before FIFO fence")
}

async fn assert_no_authority(
    daemon: &AttachedDaemon,
    client: &mut AttachedClient,
    item: &str,
    phase: &str,
) {
    let (selected, inventory) = inspect_context(client).await;
    assert!(
        selected.is_empty(),
        "{phase} selected context: {selected:?}"
    );
    assert!(
        !inventory.contains(item),
        "{phase} placed the item in selected-context inventory: {inventory}"
    );
    assert_eq!(
        daemon.provider_request_count(),
        0,
        "{phase} requested provider work"
    );
    assert_eq!(daemon.model_turn_count(), 0, "{phase} started a model turn");
}

async fn set_context(client: &mut AttachedClient, item: &str, selected: bool) {
    client
        .send_frame(ClientFrame::SetRecipientContext {
            item_id: item.into(),
            selected,
        })
        .await;
    let action = if selected {
        RecipientAction::ContextSelect
    } else {
        RecipientAction::ContextRemove
    };
    let result = client
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::RecipientActionResult {
                action: actual,
                target: RecipientActionTarget::Item { item_id },
                ..
            } if actual == &action && item_id == item)
        })
        .await;
    assert!(
        matches!(result, DaemonFrame::RecipientActionResult {
        outcome: RecipientActionOutcome::ContextSelection { selected: actual, changed: true },
        ..
    } if actual == selected),
        "explicit context positive control: {result:?}"
    );
}

#[tokio::test]
async fn k05_listed_and_legacy_wait_arrival_open_ack_grant_nothing() {
    for (task, carrier) in [
        ("wait-listed-authority", Some(listed_carrier())),
        ("wait-legacy-authority", None),
    ] {
        let has_list = carrier.is_some();
        let mut recipient = StoryRecipient::start(ResponseMode::NotifyAndWait).await;
        let address = recipient
            .send(task, "sender-stated stories start no work", carrier)
            .await;
        let item = address.item().as_str();
        recipient
            .wait_control(&address, PeerRowStage::AwaitingOperator)
            .await;
        let pending = oldest_pending(&recipient.view).expect("wait-mode arrival card");
        assert_eq!(pending.item_address.as_ref(), Some(&address));
        assert_eq!(pending.stage, PeerRowStage::AwaitingOperator);
        let before = folded_stories(&recipient.daemon, &address).await;
        assert_eq!(
            matches!(before.as_ref(), Some(ReceivedStories::Listed { .. })),
            has_list,
            "the oracle observes a real folded list, not two absent controls"
        );
        assert_no_authority(&recipient.daemon, &mut recipient.client, item, "arrival").await;

        // The actual /team view parser and production surface load this daemon's
        // journal; HistoryRequest alone would not exercise opening the item.
        let state =
            super::k07_k08_recipient::local_view(recipient.daemon.journal.workspace.path(), item)
                .await;
        let surface = state.recipient_surface.expect("opened real received item");
        assert_eq!(surface.item_state, Some("received"));
        assert!(surface.context_available && !surface.context_selected);
        assert_no_authority(&recipient.daemon, &mut recipient.client, item, "opening").await;

        recipient
            .client
            .send_frame(ClientFrame::AcknowledgeRecipientItem {
                item_id: item.into(),
            })
            .await;
        let result = recipient
            .client
            .wait_for(|frame| {
                matches!(frame, DaemonFrame::RecipientActionResult {
                    action: RecipientAction::Acknowledge,
                    target: RecipientActionTarget::Item { item_id },
                    ..
                } if item_id == item)
            })
            .await;
        assert!(
            matches!(
                result,
                DaemonFrame::RecipientActionResult {
                    outcome: RecipientActionOutcome::Applied { reply: None },
                    ..
                }
            ),
            "fresh acknowledgement must be applied: {result:?}"
        );
        assert_no_authority(
            &recipient.daemon,
            &mut recipient.client,
            item,
            "acknowledgement",
        )
        .await;
        assert_eq!(folded_stories(&recipient.daemon, &address).await, before);
        let after =
            super::k07_k08_recipient::local_view(recipient.daemon.journal.workspace.path(), item)
                .await
                .recipient_surface
                .expect("acknowledged item remains openable");
        assert_eq!(after.item_state, Some("acknowledged"));
        assert!(after.context_available && !after.context_selected);

        // Prove that both independent selection observations can detect actual
        // selection, without starting a model turn or mutating the story list.
        set_context(&mut recipient.client, item, true).await;
        let (selected, inventory) = inspect_context(&mut recipient.client).await;
        assert_eq!(selected, vec![item.to_owned()]);
        assert!(
            inventory.contains(item),
            "selection oracle did not fire: {inventory}"
        );
        set_context(&mut recipient.client, item, false).await;
        assert_no_authority(
            &recipient.daemon,
            &mut recipient.client,
            item,
            "explicit deselection",
        )
        .await;
        assert_eq!(folded_stories(&recipient.daemon, &address).await, before);
        recipient.stop().await;
    }
}

#[tokio::test]
async fn k05_draft_provider_positive_control_is_identical_with_or_without_stories() {
    for (task, carrier) in [
        ("draft-listed-authority", Some(listed_carrier())),
        ("draft-legacy-authority", None),
    ] {
        let mut recipient = StoryRecipient::start(ResponseMode::NotifyAndDraft).await;
        let address = recipient
            .send(
                task,
                "draft this receipt under the configured policy",
                carrier,
            )
            .await;
        recipient.wait_control(&address, PeerRowStage::Ready).await;
        let pending = oldest_pending(&recipient.view).expect("draft approval card");
        assert_eq!(pending.item_address.as_ref(), Some(&address));
        assert_eq!(pending.stage, PeerRowStage::Ready);
        assert_eq!(recipient.daemon.provider_request_count(), 1);
        assert_eq!(recipient.daemon.model_turn_count(), 1);
        let (selected, inventory) = inspect_context(&mut recipient.client).await;
        assert!(selected.is_empty());
        assert!(!inventory.contains(address.item().as_str()));
        recipient.stop().await;
    }
}
