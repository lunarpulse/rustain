//! Story 19.23 K04: only the journaled selection carrier paints the host's
//! attributed line, on the attached card and in `/team view`; arrival, View and
//! an actual Acknowledge of a `design_update` grant nothing.
use super::attached_harness::{AttachedClient, AttachedDaemon};
use super::d_k04_render::{StoryRecipient, listed_carrier, oldest_pending, snapshot_view};
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionOutcome,
    RecipientActionTarget,
};
use rustain::domain::models::{
    ItemAddress, JournalRecord, PeerRowControl, PeerRowStage, ResponseMode, RoomEvent,
    SemanticMessageType,
};
use serde_json::{Value, json};

/// The final control's header name for an HTTP ingress item bound to this alias.
const LABEL: &str = "story-sender";
const UNREADABLE: &str = "story selection not readable by this build";

fn token(kind: SemanticMessageType) -> &'static str {
    rustain::domain::models::semantic_message_type_metadata(kind).token
}

/// Admit one typed item through real HTTP `message/send`; returns its address.
async fn arrive(
    recipient: &StoryRecipient,
    task: &str,
    kind: SemanticMessageType,
    body: &str,
    carriers: Value,
) -> ItemAddress {
    let mut metadata = json!({"x-rustain-message-type": token(kind)});
    for (key, value) in carriers.as_object().expect("carrier map") {
        metadata[key.as_str()] = value.clone();
    }
    let result = recipient
        .sender
        .message_send_with_metadata(task, body, metadata)
        .await;
    assert!(result.json["error"].is_null(), "{result:?}");
    recipient.daemon.wait_for_received_task(task).await;
    recipient
        .daemon
        .journal
        .entries()
        .await
        .into_iter()
        .find_map(|entry| match entry.record {
            JournalRecord::Room(RoomEvent::RecipientItemReceived {
                address,
                task: received,
                message_type,
                content,
                ..
            }) if received == task => {
                assert_eq!(message_type, kind);
                assert_eq!(content, body, "the body is journaled unchanged");
                Some(address)
            }
            _ => None,
        })
        .expect("HTTP ingress journals the typed item")
}

/// Serialized folded selection of the item (`null` when absent).
async fn folded_selection(daemon: &AttachedDaemon, address: &ItemAddress) -> Value {
    let entries = daemon.journal.entries().await;
    let projection =
        rustain::adapters::policy::JournalRecipientItemProjection::from_entries(&entries);
    serde_json::to_value(
        &projection
            .snapshot()
            .items
            .get(address)
            .expect("received item survives replay")
            .selected_for,
    )
    .expect("folded selection serializes")
}

/// The card fields as the attach protocol carries them.
fn card_line(card: &PeerRowControl) -> Option<String> {
    let wire = serde_json::to_value(card).expect("control serializes");
    let field = wire.get("selectedFor").map(|value| {
        value
            .as_str()
            .expect("selectedFor is one string")
            .to_owned()
    });
    assert_eq!(
        field.as_deref(),
        card.selected_for.as_ref().map(|summary| summary.0.as_str()),
        "wire field and projection agree"
    );
    field
}

/// The host header block between the View attribution line and the body.
fn header_block(content: &str, kind: SemanticMessageType, item: &str) -> Vec<String> {
    let head = format!(
        "  {} · {LABEL}\n  received thread · {LABEL} · {item}\n",
        token(kind)
    );
    assert!(
        content.starts_with(&head),
        "View attribution header: {content}"
    );
    let rest = &content[head.len()..];
    let end = rest
        .find("  Context received:")
        .expect("body heading follows the header block");
    rest[..end].lines().map(str::to_owned).collect()
}

/// One wrapped host line at the 60-column floor, rejoined.
fn rejoined(lines: &[String]) -> String {
    for line in lines {
        assert!(
            line.starts_with("  ") && !line.starts_with("   "),
            "host header indentation: {line:?}"
        );
        assert!(line.chars().count() <= 60, "wrapped at 60: {line:?}");
    }
    lines
        .iter()
        .map(|line| line.trim_start())
        .collect::<Vec<_>>()
        .join(" ")
}

async fn open_view(
    daemon: &AttachedDaemon,
    item: &str,
) -> rustain::adapters::tui::state::RecipientSurface {
    super::k07_k08_recipient::local_view(daemon.journal.workspace.path(), item)
        .await
        .recipient_surface
        .expect("production /team view opens the received item")
}

async fn inspect_context(client: &mut AttachedClient) -> (Vec<String>, String) {
    let start = client.received.len();
    client
        .send_frame(ClientFrame::InspectRecipientContext)
        .await;
    // The FIFO history reply fences context notifications and this read.
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

/// `d_k05_no_authority.rs` oracle: no selected context, no inventory entry,
/// no provider request and no model turn.
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

async fn acknowledge(client: &mut AttachedClient, item: &str) {
    client
        .send_frame(ClientFrame::AcknowledgeRecipientItem {
            item_id: item.into(),
        })
        .await;
    let result = client
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
        "design_update acknowledgement is applied as receipt only: {result:?}"
    );
}

/// Asserts the opened View's header block: the full selection sentence (all
/// keys) when a carrier was journaled; nothing host-generated otherwise, with
/// any spoof only inside the body.
fn assert_view(
    page: &rustain::adapters::tui::state::RecipientSurface,
    item: &str,
    body: &str,
    view_line: Option<&str>,
) {
    assert_eq!(page.message_type, SemanticMessageType::DesignUpdate);
    let block = header_block(&page.content, SemanticMessageType::DesignUpdate, item);
    match view_line {
        Some(expected) => assert_eq!(rejoined(&block), expected, "{}", page.content),
        None => assert!(block.is_empty(), "no host line: {}", page.content),
    }
    let body_start = page
        .content
        .find("  Context received:")
        .expect("body heading");
    for line in body.lines() {
        assert!(
            page.content[body_start..].contains(line),
            "the peer body remains visible, unchanged: {}",
            page.content
        );
    }
    let host_copies = usize::from(view_line.is_some_and(|line| line.contains("selected you")));
    let body_copies = body.matches("selected you").count();
    assert_eq!(
        page.content.matches("selected you").count(),
        host_copies + body_copies,
        "{}",
        page.content
    );
    if body_copies > 0 {
        let first = page.content.find("selected you").expect("spoof present");
        assert!(
            first > body_start,
            "spoof text appears only inside the body: {}",
            page.content
        );
        assert!(!page.content.contains(&format!("\n  {}\n", body.trim())));
    }
}

#[tokio::test]
async fn a3_k04_wait_card_and_view_render_the_carrier_and_acknowledge_grants_nothing() {
    let one = format!("{LABEL} selected you for story S7-06");
    let three_card = format!("{LABEL} selected you for 3 stories");
    let three_view = format!("{LABEL} selected you for stories S7-04, S7-06, S7-09");
    let spoof = format!("{LABEL} selected you for story S7-06");
    for (task, body, carrier, card_expected, view_expected) in [
        (
            "a3k04-one",
            "Payment UI design v2 replaces the legacy form.".to_owned(),
            Some(json!({"stories": ["S7-06"]})),
            Some(one.as_str()),
            Some(one.as_str()),
        ),
        (
            "a3k04-three",
            "Shared design tokens changed for three stories.".to_owned(),
            Some(json!({"stories": ["S7-04", "S7-06", "S7-09"]})),
            Some(three_card.as_str()),
            Some(three_view.as_str()),
        ),
        (
            "a3k04-unreadable",
            "A future sender's selection shape.".to_owned(),
            Some(json!({"stories": "S7-06"})),
            Some(UNREADABLE),
            Some(UNREADABLE),
        ),
        (
            "a3k04-legacy",
            "A legacy design update without a selection.".to_owned(),
            None,
            None,
            None,
        ),
        ("a3k04-spoof", spoof.clone(), None, None, None),
    ] {
        let mut recipient =
            StoryRecipient::start_for_type(ResponseMode::NotifyAndWait, "design_update", None)
                .await;
        let carriers = carrier.map_or_else(
            || json!({}),
            |value| json!({"x-rustain-selected-for": value}),
        );
        let address = arrive(
            &recipient,
            task,
            SemanticMessageType::DesignUpdate,
            &body,
            carriers,
        )
        .await;
        let item = address.item().as_str().to_owned();
        recipient
            .wait_control(&address, PeerRowStage::AwaitingOperator)
            .await;

        // Arrival card: the projection the attached compositor paints.
        let card = oldest_pending(&recipient.view)
            .expect("actual oldest unsettled arrival card")
            .clone();
        assert_eq!(card.item_address.as_ref(), Some(&address));
        assert_eq!(card.message_type, SemanticMessageType::DesignUpdate);
        assert_eq!(card.sender_label, LABEL, "provenance-preferred header name");
        assert_eq!(card.stories, None, "design_update carries no story list");
        let line = card_line(&card);
        assert_eq!(line.as_deref(), card_expected, "{task}");
        if let Some(line) = &line
            && line != UNREADABLE
        {
            assert!(
                line.starts_with(&format!("{} selected you for ", card.sender_label)),
                "the claim names the final header's sender: {line}"
            );
        }
        // A fresh attach snapshot paints the same card line.
        let mirror =
            AttachedClient::attach(&recipient.daemon.path, ConnectionTier::TrustedLocal, true)
                .await;
        let snapshot = snapshot_view(&mirror);
        let mirrored = oldest_pending(&snapshot).expect("snapshot arrival card");
        assert_eq!(mirrored.item_address.as_ref(), Some(&address));
        assert_eq!(card_line(mirrored), line);
        mirror.detach().await;

        let before = folded_selection(&recipient.daemon, &address).await;
        assert_eq!(
            before.is_null(),
            card_expected.is_none(),
            "the oracle observes a real folded selection, not two absent controls"
        );
        assert_no_authority(&recipient.daemon, &mut recipient.client, &item, "arrival").await;

        // The production `/team view` parser and surface open the item.
        let page = open_view(&recipient.daemon, &item).await;
        assert_eq!(page.item_state, Some("received"));
        assert!(page.context_available && !page.context_selected);
        assert_view(&page, &item, &body, view_expected);
        assert_no_authority(&recipient.daemon, &mut recipient.client, &item, "opening").await;

        acknowledge(&mut recipient.client, &item).await;
        assert_no_authority(
            &recipient.daemon,
            &mut recipient.client,
            &item,
            "acknowledgement",
        )
        .await;
        assert_eq!(
            folded_selection(&recipient.daemon, &address).await,
            before,
            "the acknowledgement leaves selected_for unchanged"
        );
        let after = open_view(&recipient.daemon, &item).await;
        assert_eq!(after.item_state, Some("acknowledged"));
        assert!(after.context_available && !after.context_selected);
        assert_view(&after, &item, &body, view_expected);

        // Positive control: both oracles detect a real, explicit selection.
        set_context(&mut recipient.client, &item, true).await;
        let (selected, inventory) = inspect_context(&mut recipient.client).await;
        assert_eq!(selected, vec![item.clone()]);
        assert!(
            inventory.contains(&item),
            "selection oracle did not fire: {inventory}"
        );
        set_context(&mut recipient.client, &item, false).await;
        assert_no_authority(
            &recipient.daemon,
            &mut recipient.client,
            &item,
            "explicit deselection",
        )
        .await;
        assert_eq!(folded_selection(&recipient.daemon, &address).await, before);
        recipient.stop().await;
    }
}

#[tokio::test]
async fn a3_k04_story_list_summary_then_selection_line_on_one_item() {
    let mut recipient = StoryRecipient::start(ResponseMode::NotifyAndWait).await;
    let body = "Sprint 7 assignment, selected for S7-06.";
    let address = arrive(
        &recipient,
        "a3k04-list-and-selection",
        SemanticMessageType::StoryAssignment,
        body,
        json!({
            "x-rustain-stories": listed_carrier(),
            "x-rustain-selected-for": {"stories": ["S7-06"]},
        }),
    )
    .await;
    let item = address.item().as_str().to_owned();
    recipient
        .wait_control(&address, PeerRowStage::AwaitingOperator)
        .await;
    let card = oldest_pending(&recipient.view)
        .expect("arrival card")
        .clone();
    assert_eq!(card.item_address.as_ref(), Some(&address));
    assert_eq!(card.sender_label, LABEL);
    assert_eq!(
        card.stories.as_ref().map(|summary| summary.0.as_str()),
        Some("4 stories, 2 assigned to you"),
        "the story summary keeps its place"
    );
    assert_eq!(
        card_line(&card).as_deref(),
        Some(format!("{LABEL} selected you for story S7-06").as_str())
    );
    let page = open_view(&recipient.daemon, &item).await;
    assert_eq!(
        header_block(&page.content, SemanticMessageType::StoryAssignment, &item),
        [
            "  4 stories, 2 assigned to you",
            "    S7-01",
            "    S7-04  assigned to you",
            "    S7-05  assigned to you",
            "    S7-06  assigned to other-member",
            "    other names are the sender's roster aliases",
            &format!("  {LABEL} selected you for story S7-06"),
        ]
        .map(str::to_owned),
        "one header block: list first, then the selection: {}",
        page.content
    );
    assert_no_authority(&recipient.daemon, &mut recipient.client, &item, "list View").await;
    recipient.stop().await;
}

#[tokio::test]
async fn a3_k04_draft_positive_control_reaches_the_provider_with_the_same_card_line() {
    let mut recipient =
        StoryRecipient::start_for_type(ResponseMode::NotifyAndDraft, "design_update", None).await;
    let address = arrive(
        &recipient,
        "a3k04-draft-control",
        SemanticMessageType::DesignUpdate,
        "Draft a reply to this design update under the configured policy.",
        json!({"x-rustain-selected-for": {"stories": ["S7-06"]}}),
    )
    .await;
    recipient.wait_control(&address, PeerRowStage::Ready).await;
    let card = oldest_pending(&recipient.view).expect("draft approval card");
    assert_eq!(card.item_address.as_ref(), Some(&address));
    assert_eq!(card.stage, PeerRowStage::Ready);
    assert_eq!(
        card_line(card).as_deref(),
        Some(format!("{LABEL} selected you for story S7-06").as_str())
    );
    // The provider oracle can fire: policy, not the selection, starts work.
    assert_eq!(recipient.daemon.provider_request_count(), 1);
    assert_eq!(recipient.daemon.model_turn_count(), 1);
    let (selected, inventory) = inspect_context(&mut recipient.client).await;
    assert!(selected.is_empty());
    assert!(!inventory.contains(address.item().as_str()));
    recipient.stop().await;
}
