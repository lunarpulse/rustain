//! Story 19.21d K04: only the journaled carrier supplies the attached card and View.
use std::sync::Arc;

use super::attached_harness::{
    AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT, InboundHttpHost, InboundHttpSender,
    InboundSenderIdentity, pinned_peer_for_inbound_sender,
};
use super::harness::Journal;
use rustain::adapters::daemon::protocol::{ConnectionTier, DaemonFrame};
use rustain::adapters::daemon::server::TeamSendTestHooks;
use rustain::domain::models::{
    Conversation, IndividualDefaults, IndividualPolicy, ItemAddress, NotificationUrgency,
    PeerRowControl, PeerRowStage, PrincipalKind, ReceivedStories, ResponseMode, RoomEvent,
    SemanticMessageType, TeamPolicy, TeamTypeOverride,
};
use rustain::infrastructure::runtime::attach_loop::{apply_peer_row_operation, seed_conversation};
use serde_json::{Value, json};

pub(super) struct StoryRecipient {
    pub daemon: AttachedDaemon,
    pub host: InboundHttpHost,
    pub sender: InboundHttpSender,
    pub client: AttachedClient,
    pub view: Conversation,
    pub policy: Arc<rustain::domain::models::EffectivePolicy>,
    cursor: usize,
}

impl StoryRecipient {
    pub async fn start(mode: ResponseMode) -> Self {
        Self::start_for_type(mode, "story_assignment", None).await
    }

    pub async fn start_status(mode: ResponseMode, level: &str) -> Self {
        Self::start_for_type(mode, "status_request", Some(level)).await
    }

    pub async fn start_for_type(
        mode: ResponseMode,
        message_type: &str,
        level: Option<&str>,
    ) -> Self {
        Self::start_for_type_with_norm(mode, message_type, level, None).await
    }

    pub async fn start_for_type_with_norm(
        mode: ResponseMode,
        message_type: &str,
        level: Option<&str>,
        norm: Option<&str>,
    ) -> Self {
        let identity = InboundSenderIdentity::new("story-sender");
        let peer_id = identity.identity.peer_id.clone();
        let peers = vec![pinned_peer_for_inbound_sender(&identity)];
        let journal = Journal::open().await;
        // A standing, pinned grant avoids creating an older consent-setup item
        // which would otherwise own the FIFO arrival card.
        journal
            .room
            .record_event(RoomEvent::ConsentGranted {
                sender: Some(peer_id.clone()),
                granted_at: 1_800_000_000_000,
                principal: Some(PrincipalKind::Pinned),
            })
            .await
            .expect("persist standing sender consent");
        let individual = IndividualPolicy {
            defaults: IndividualDefaults {
                response_mode: Some(ResponseMode::NotifyAndAuto),
                notification: Some(NotificationUrgency::Immediate),
                status_detail_minimum: level.map(str::to_owned),
                ..IndividualDefaults::default()
            },
            ..IndividualPolicy::default()
        };
        let mut team = TeamPolicy::default();
        team.overrides.status_detail_minimum = norm.map(str::to_owned);
        team.overrides.per_type.insert(
            message_type.into(),
            toml::Value::try_from(TeamTypeOverride {
                response_mode: Some(mode),
                notification: None,
            })
            .expect("story response-mode override"),
        );
        let policy = Arc::new(
            rustain::domain::services::team_policy::resolve_effective_policy(
                &individual,
                Some(&team),
                &peers,
            ),
        );
        let room = journal.room.clone();
        let daemon = AttachedDaemon::compose_with_clock_and_test_hooks_and_policy(
            journal,
            peers,
            room,
            Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
                1_800_000_000_000,
            )),
            TeamSendTestHooks::new(None, None, false),
            Some(policy.clone()),
        )
        .await;
        daemon.wait_for_pinned_sender_consent(&peer_id).await;
        let host = daemon.serve_http(vec![identity]).await;
        let sender = host.sender("story-sender");
        let client =
            AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
        let view = snapshot_view(&client);
        Self {
            daemon,
            host,
            sender,
            client,
            view,
            policy,
            cursor: 0,
        }
    }

    pub async fn send(&self, task: &str, body: &str, stories: Option<Value>) -> ItemAddress {
        let mut metadata = json!({"x-rustain-message-type": "story_assignment"});
        if let Some(stories) = stories {
            metadata["x-rustain-stories"] = stories;
        }
        let result = self
            .sender
            .message_send_with_metadata(task, body, metadata)
            .await;
        assert!(result.json["error"].is_null(), "{result:?}");
        assert!(
            result.json["result"]["metadata"]["x-rustain-item-id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "wait/draft auth-required with a real item is admitted: {result:?}"
        );
        self.daemon.wait_for_received_task(task).await;
        self.daemon
            .journal
            .entries()
            .await
            .into_iter()
            .find_map(|entry| match entry.record {
                rustain::domain::models::JournalRecord::Room(
                    RoomEvent::RecipientItemReceived {
                        address,
                        task: received,
                        message_type,
                        ..
                    },
                ) if received == task => {
                    assert_eq!(message_type, SemanticMessageType::StoryAssignment);
                    Some(address)
                }
                _ => None,
            })
            .expect("HTTP ingress journals the typed item")
    }

    pub async fn wait_control(&mut self, address: &ItemAddress, stage: PeerRowStage) {
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                for frame in &self.client.received[self.cursor..] {
                    if let DaemonFrame::PeerRowOperation(operation) = frame {
                        apply_peer_row_operation(&mut self.view, operation.as_ref().clone());
                    }
                }
                self.cursor = self.client.received.len();
                if self.view.peer_row_controls.values().any(|control| {
                    control.item_address.as_ref() == Some(address) && control.stage == stage
                }) {
                    return;
                }
                self.client.collect_one().await;
            }
        })
        .await
        .expect("live attached projection reaches the item's response stage");
    }

    pub async fn stop(self) {
        self.client.detach().await;
        self.host.stop().await;
        self.daemon.stop().await;
    }
}

pub(super) fn listed_carrier() -> Value {
    json!({"stories": [
        {"key": "S7-01"},
        {"key": "S7-04", "you": true},
        {"key": "S7-05", "you": true},
        {"key": "S7-06", "alias": "other-member"}
    ]})
}

pub(super) fn snapshot_view(client: &AttachedClient) -> Conversation {
    client
        .received
        .iter()
        .find_map(|frame| match frame {
            DaemonFrame::AttachAck { snapshot, .. } => Some(seed_conversation(snapshot)),
            _ => None,
        })
        .expect("real signed attach snapshot")
}

// Match the attached compositor's FIFO boundary: message order, not control-map
// order, and never a Settled row. An arbitrary operation is not a pending card.
pub(super) fn oldest_pending(view: &Conversation) -> Option<&PeerRowControl> {
    view.messages.iter().find_map(|message| {
        view.peer_row_controls
            .get(&message.id)
            .filter(|control| control.stage != PeerRowStage::Settled)
    })
}

pub(super) async fn folded_stories(
    daemon: &AttachedDaemon,
    address: &ItemAddress,
) -> Option<ReceivedStories> {
    let entries = daemon.journal.entries().await;
    let projection =
        rustain::adapters::policy::JournalRecipientItemProjection::from_entries(&entries);
    projection
        .snapshot()
        .items
        .get(address)
        .expect("received item survives replay")
        .stories
        .clone()
}

#[tokio::test]
async fn k04_card_and_opened_view_use_the_carrier_not_the_body() {
    for (task, body, carrier, summary) in [
        (
            "listed-render",
            "8 stories, 2 assigned to you",
            Some(listed_carrier()),
            Some("4 stories, 2 assigned to you"),
        ),
        ("legacy-render", "legacy typed receipt", None, None),
        (
            "spoof-render",
            "8 stories, 2 assigned to you\nS7-04  assigned to you",
            None,
            None,
        ),
    ] {
        let mut recipient = StoryRecipient::start(ResponseMode::NotifyAndWait).await;
        let address = recipient.send(task, body, carrier).await;
        recipient
            .wait_control(&address, PeerRowStage::AwaitingOperator)
            .await;
        let card = oldest_pending(&recipient.view).expect("actual oldest unsettled arrival card");
        assert_eq!(card.item_address.as_ref(), Some(&address));
        assert_eq!(card.message_type, SemanticMessageType::StoryAssignment);
        assert_eq!(card.sender_label, "story-sender");
        assert_eq!(
            card.stories.as_ref().map(|summary| summary.0.as_str()),
            summary
        );

        // A fresh mirror must paint the same card from the actual attach snapshot.
        let mirror =
            AttachedClient::attach(&recipient.daemon.path, ConnectionTier::TrustedLocal, true)
                .await;
        let snapshot = snapshot_view(&mirror);
        let snapshot_card = oldest_pending(&snapshot).expect("snapshot arrival card");
        assert_eq!(snapshot_card.item_address.as_ref(), Some(&address));
        assert_eq!(snapshot_card.stories, card.stories);

        let state = super::k07_k08_recipient::local_view(
            recipient.daemon.journal.workspace.path(),
            address.item().as_str(),
        )
        .await;
        let page = state.recipient_surface.expect("production /team view page");
        assert_eq!(page.item_state, Some("received"));
        for line in body.lines() {
            assert!(page.content.contains(line), "the peer body remains visible");
        }
        if let Some(summary) = summary {
            let block = format!(
                "  {summary}\n    S7-01\n    S7-04  assigned to you\n    S7-05  assigned to you\n    S7-06  assigned to other-member\n    other names are the sender's roster aliases\n"
            );
            assert!(page.content.contains(&block), "{}", page.content);
            assert!(
                page.content.find(&block).expect("host story block")
                    < page
                        .content
                        .find("Context received")
                        .expect("context heading")
            );
        } else {
            assert!(folded_stories(&recipient.daemon, &address).await.is_none());
            // The spoof remains ordinary body text, never an indented host block.
            assert!(!page.content.contains("\n  8 stories, 2 assigned to you\n"));
            assert!(!page.content.contains("\n    S7-04  assigned to you\n"));
            assert!(
                !page
                    .content
                    .contains("other names are the sender's roster aliases")
            );
        }
        assert_eq!(recipient.daemon.provider_request_count(), 0);
        mirror.detach().await;
        recipient.stop().await;
    }
}

#[tokio::test]
async fn k04_auto_with_or_without_a_list_keeps_no_pending_card() {
    for (task, carrier) in [
        ("auto-listed", Some(listed_carrier())),
        ("auto-legacy", None),
    ] {
        let mut recipient = StoryRecipient::start(ResponseMode::NotifyAndAuto).await;
        let address = recipient
            .send(task, "automatic typed receipt", carrier)
            .await;
        recipient
            .wait_control(&address, PeerRowStage::Settled)
            .await;
        assert!(oldest_pending(&recipient.view).is_none());
        assert_eq!(
            recipient.daemon.provider_request_count(),
            0,
            "NotifyAndAuto uses its conservative template, never a provider"
        );
        let mirror =
            AttachedClient::attach(&recipient.daemon.path, ConnectionTier::TrustedLocal, true)
                .await;
        assert!(oldest_pending(&snapshot_view(&mirror)).is_none());
        mirror.detach().await;
        recipient.stop().await;
    }
}
