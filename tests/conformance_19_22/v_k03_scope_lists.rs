//! Story 19.22 K03: story lists ride `scope_change` at every gate, and a
//! holder-less entry on a `scope_change` reads as a release, not a listing.
//!
//! Front doors only: raw HTTP `message/send` ingress with the real carrier
//! metadata, the attached `/team send` frame fanned out over real
//! `message/send`, the live attached card projection, and the served
//! `/team view` page. RED on the 19.24 baseline, where a `scope_change`
//! carrier is ignored at `parse_story_carrier`.
use std::sync::Arc;

use super::attached_harness::{AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT, RecipientHost};
use super::d_k04_render::{StoryRecipient, oldest_pending, snapshot_view};
use super::harness::{Journal, read_room_events};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{ConnectionTier, DaemonFrame, TeamSendOutcome};
use rustain::domain::models::{
    Conversation, ItemAddress, PeerRowStage, ReceivedStories, RoomEvent, SemanticMessageType,
    StoryHolder, StoryManifest,
};
use rustain::infrastructure::runtime::attach_loop::apply_peer_row_operation;
use serde_json::{Value, json};

fn notify_and_wait_policy() -> Arc<rustain::domain::models::EffectivePolicy> {
    Arc::new(
        rustain::domain::services::team_policy::resolve_effective_policy(
            &rustain::domain::models::IndividualPolicy::default(),
            None,
            &[],
        ),
    )
}

async fn received_address(journal: &Journal, task: &str) -> ItemAddress {
    read_room_events(journal)
        .await
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address,
                task: received,
                ..
            } if received == task => Some(address),
            _ => None,
        })
        .unwrap_or_else(|| panic!("ingress for {task:?}"))
}

/// Attached `/team send` fan-out mints remote task ids; find the item by its
/// delivered body instead.
async fn received_address_by_content(journal: &Journal, content: &str) -> ItemAddress {
    read_room_events(journal)
        .await
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address,
                content: body,
                ..
            } if body == content => Some(address),
            _ => None,
        })
        .unwrap_or_else(|| panic!("ingress delivering {content:?}"))
}

async fn folded(journal: &Journal, address: &ItemAddress) -> Option<ReceivedStories> {
    let entries = journal.entries().await;
    rustain::adapters::policy::JournalRecipientItemProjection::from_entries(&entries)
        .snapshot()
        .items
        .get(address)
        .expect("received item survives replay")
        .stories
        .clone()
}

fn listed_entries(stories: &ReceivedStories) -> Vec<(String, StoryHolder)> {
    match stories {
        ReceivedStories::Listed { stories } => stories
            .iter()
            .map(|story| (story.key.to_string(), story.holder.clone()))
            .collect(),
        other => panic!("expected a listed carrier, got {other:?}"),
    }
}

/// Track the live attached card projection until the item's control reaches
/// `stage`, exactly like the compositor: message order, applied operations.
async fn wait_card(
    client: &mut AttachedClient,
    view: &mut Conversation,
    address: &ItemAddress,
    stage: PeerRowStage,
) {
    let mut cursor = 0usize;
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            for frame in &client.received[cursor..] {
                if let DaemonFrame::PeerRowOperation(operation) = frame {
                    apply_peer_row_operation(view, operation.as_ref().clone());
                }
            }
            cursor = client.received.len();
            if view.peer_row_controls.values().any(|control| {
                control.item_address.as_ref() == Some(address) && control.stage == stage
            }) {
                return;
            }
            client.collect_one().await;
        }
    })
    .await
    .expect("live attached projection reaches the card stage");
}

fn scope_carrier() -> Value {
    json!({"stories": [
        {"key": "S7-09", "you": true},
        {"key": "S7-07"}
    ]})
}

#[tokio::test]
async fn v_k03_scope_change_carries_story_lists() {
    // ---- Raw-HTTP leg (the Task 0 RED): carrier metadata on `scope_change` ----
    let mut scope = StoryRecipient::start_for_type(
        rustain::domain::models::ResponseMode::NotifyAndWait,
        "scope_change",
        None,
    )
    .await;
    let mut metadata = json!({"x-rustain-message-type": "scope_change"});
    metadata["x-rustain-stories"] = scope_carrier();
    let result = scope
        .sender
        .message_send_with_metadata("scope-listed", "Sprint 7 scope change", metadata)
        .await;
    assert!(result.json["error"].is_null(), "{result:?}");
    scope.daemon.wait_for_received_task("scope-listed").await;
    let scope_address = received_address(&scope.daemon.journal, "scope-listed").await;
    let folded_scope = folded(&scope.daemon.journal, &scope_address).await;
    let entries = listed_entries(folded_scope.as_ref().expect("scope_change stores the list"));
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "S7-09");
    assert!(matches!(entries[0].1, StoryHolder::You));
    assert_eq!(entries[1].0, "S7-07");
    assert!(matches!(entries[1].1, StoryHolder::NoHolder));

    scope
        .wait_control(&scope_address, PeerRowStage::AwaitingOperator)
        .await;
    let card = oldest_pending(&scope.view).expect("scope_change arrival card");
    assert_eq!(card.item_address.as_ref(), Some(&scope_address));
    assert_eq!(
        card.stories.as_ref().map(|summary| summary.0.as_str()),
        Some("2 stories, 1 assigned to you")
    );
    let page = super::k07_k08_recipient::local_view(
        scope.daemon.journal.workspace.path(),
        scope_address.item().as_str(),
    )
    .await;
    let content = page
        .recipient_surface
        .expect("served /team view page")
        .content;
    assert!(
        content.contains("    S7-09  assigned to you\n"),
        "{content}"
    );
    assert!(
        content.contains("    S7-07  assigned to no one\n"),
        "a holder-less scope_change entry reads as released: {content}"
    );
    assert_eq!(scope.daemon.provider_request_count(), 0);
    scope.stop().await;

    // ---- Control: the same carrier on `design_update` is ignored ----
    let design = StoryRecipient::start_for_type(
        rustain::domain::models::ResponseMode::NotifyAndWait,
        "design_update",
        None,
    )
    .await;
    let mut design_metadata = json!({"x-rustain-message-type": "design_update"});
    design_metadata["x-rustain-stories"] = scope_carrier();
    let result = design
        .sender
        .message_send_with_metadata("design-spoof", "8 stories…", design_metadata)
        .await;
    assert!(result.json["error"].is_null(), "{result:?}");
    design.daemon.wait_for_received_task("design-spoof").await;
    let design_address = received_address(&design.daemon.journal, "design-spoof").await;
    assert!(
        folded(&design.daemon.journal, &design_address)
            .await
            .is_none(),
        "a design_update never stores a story list"
    );
    design.stop().await;

    // ---- Attached fan-out leg: one operator send, two real recipient hosts ----
    let mut b = RecipientHost::start_with_effective_policy(
        A2aAdmissionPolicy::Allow,
        notify_and_wait_policy(),
    )
    .await;
    let c = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let pin_b = b
        .pinned_peer("B")
        .pinned_identity()
        .expect("actual B signer pin");
    let socket = b.enable_operator().await;
    let mut b_operator = AttachedClient::attach(&socket, ConnectionTier::TrustedLocal, false).await;
    let mut b_view = snapshot_view(&b_operator);
    let daemon = AttachedDaemon::start(vec![b.pinned_peer("B"), c.pinned_peer("C")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    writer
        .send_team(
            "scope-broadcast",
            vec!["B".into(), "C".into()],
            "Sprint 7 scope change",
            Some(SemanticMessageType::ScopeChange),
            None,
            Some(StoryManifest::parse("S7-09=B,S7-07").unwrap()),
        )
        .await;
    for index in 0..2 {
        assert!(matches!(
            writer.wait_send_settled("scope-broadcast", index).await,
            DaemonFrame::TeamSendSettled {
                outcome: TeamSendOutcome::Delivered,
                ..
            }
        ));
    }
    let b_address = received_address_by_content(&b.journal, "Sprint 7 scope change").await;
    let c_address = received_address_by_content(&c.journal, "Sprint 7 scope change").await;
    let b_entries = listed_entries(
        &folded(&b.journal, &b_address)
            .await
            .expect("B stores the scope_change list"),
    );
    assert_eq!(b_entries.len(), 2);
    assert_eq!(b_entries[0].0, "S7-09");
    assert!(matches!(b_entries[0].1, StoryHolder::You));
    assert!(matches!(b_entries[1].1, StoryHolder::NoHolder));
    let c_entries = listed_entries(
        &folded(&c.journal, &c_address)
            .await
            .expect("C stores the scope_change list"),
    );
    assert_eq!(c_entries[0].0, "S7-09");
    match &c_entries[0].1 {
        StoryHolder::Member { alias, peer } => {
            assert_eq!(alias, "B");
            assert_eq!(peer.as_ref(), Some(&pin_b));
        }
        other => panic!("C sees B as a member, got {other:?}"),
    }
    assert!(matches!(c_entries[1].1, StoryHolder::NoHolder));

    wait_card(
        &mut b_operator,
        &mut b_view,
        &b_address,
        PeerRowStage::AwaitingOperator,
    )
    .await;
    let card = b_view
        .peer_row_controls
        .values()
        .find(|control| control.item_address.as_ref() == Some(&b_address))
        .expect("B's scope_change card");
    assert_eq!(
        card.stories.as_ref().map(|summary| summary.0.as_str()),
        Some("2 stories, 1 assigned to you")
    );
    let b_page =
        super::k07_k08_recipient::local_view(b.journal.workspace.path(), b_address.item().as_str())
            .await;
    let b_content = b_page
        .recipient_surface
        .expect("B's served /team view page")
        .content;
    assert!(
        b_content.contains("    S7-07  assigned to no one\n"),
        "{b_content}"
    );

    // ---- Control: `story_assignment` rendering is byte-identical to 19.21d ----
    writer
        .send_team(
            "bare-holder-list",
            vec!["B".into()],
            "story receipt",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(StoryManifest::parse("S7-07").unwrap()),
        )
        .await;
    assert!(matches!(
        writer.wait_send_settled("bare-holder-list", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: TeamSendOutcome::Delivered,
            ..
        }
    ));
    let bare_address = received_address_by_content(&b.journal, "story receipt").await;
    let bare_page = super::k07_k08_recipient::local_view(
        b.journal.workspace.path(),
        bare_address.item().as_str(),
    )
    .await;
    let bare_content = bare_page
        .recipient_surface
        .expect("bare story_assignment page")
        .content;
    assert!(
        bare_content.contains("\n    S7-07\n"),
        "a story_assignment holder-less entry stays the bare key: {bare_content}"
    );
    assert!(
        !bare_content.contains("S7-07  assigned to no one"),
        "the release line is a scope_change rendering only: {bare_content}"
    );
    assert_eq!(daemon.provider_request_count(), 0);
    assert_eq!(b.method_count("x-rustain-items/thread"), 0);
    writer.detach().await;
    b_operator.detach().await;
    daemon.stop().await;
    b.stop().await;
    c.stop().await;
}
