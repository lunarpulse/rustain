//! Story 19.23 K05: Act 3's compare and ask beats are composition. A selected
//! `design_update` used as explicit context reaches only the recipient's own
//! provider request, inside the untrusted received-item frame, and nothing goes
//! back to the sender; the explicit consultation is the only send.
//!
//! The sender's roster endpoint and inbound bearer binding use the same actual
//! public identity; the local compare cannot send to that host silently.
use base64::Engine as _;
use std::sync::Arc;

use super::attached_harness::{
    AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT, InboundHttpHost, InboundHttpSender,
    InboundSenderIdentity, RecipientHost,
};
use super::harness::{Journal, read_room_events};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionOutcome,
    RecipientActionTarget, TeamSendOutcome,
};
use rustain::adapters::daemon::server::TeamSendTestHooks;
use rustain::domain::models::{
    IndividualDefaults, IndividualPolicy, ItemAddress, JournalRecord, MessageRole,
    NotificationUrgency, PeerRowStage, PrincipalKind, ResponseMode, RoomEvent, SemanticMessageType,
    StopReason, StreamChunk, TeamPolicy, TeamTypeOverride,
};
use rustain::infrastructure::runtime::event_bus::RawEventKind;
use serde_json::json;

const SENDER: &str = "nina";
const BODY_FIRST: &str = "Design v2: the payment UI moves to the shared checkout form.";
const BODY_SECOND: &str = "S7-06 drops the legacy card field.";
const QUESTION: &str = "Compare this design update with the current S7-06 implementation.";
const CONSULTATION: &str = "Does S7-06 keep the legacy card field until the migration ships?";

struct Fixture {
    sender_host: RecipientHost,
    daemon: AttachedDaemon,
    inbound: InboundHttpHost,
    wire: InboundHttpSender,
    client: AttachedClient,
}

impl Fixture {
    async fn start() -> Self {
        // The sender's host: R's roster entry for `nina` is pinned to the
        // actual signer of this observed HTTP endpoint.
        let sender_host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
        let peer = sender_host.pinned_peer(SENDER);
        let mut identity = InboundSenderIdentity::new(SENDER);
        identity.identity = rustain::domain::models::PeerIdentity::from_public_key(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&peer.pinned_key.as_ref().expect("real sender signer pin").x)
                .expect("decode actual sender public key"),
        )
        .expect("actual sender identity");
        let principal = identity.identity.peer_id.clone();
        assert_eq!(peer.pinned_identity().as_ref(), Some(&principal));
        let peers = vec![peer];
        let journal = Journal::open().await;
        journal
            .room
            .record_event(RoomEvent::ConsentGranted {
                sender: Some(principal.clone()),
                granted_at: 1_800_000_000_000,
                principal: Some(PrincipalKind::Pinned),
            })
            .await
            .expect("persist standing sender consent");
        let individual = IndividualPolicy {
            defaults: IndividualDefaults {
                response_mode: Some(ResponseMode::NotifyAndAuto),
                notification: Some(NotificationUrgency::Immediate),
                ..IndividualDefaults::default()
            },
            ..IndividualPolicy::default()
        };
        let mut team = TeamPolicy::default();
        team.overrides.per_type.insert(
            "design_update".into(),
            toml::Value::try_from(TeamTypeOverride {
                response_mode: Some(ResponseMode::NotifyAndWait),
                notification: None,
            })
            .expect("design_update wait override"),
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
            Some(policy),
        )
        .await;
        daemon.wait_for_pinned_sender_consent(&principal).await;
        let inbound = daemon.serve_http(vec![identity]).await;
        let wire = inbound.sender(SENDER);
        let client =
            AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
        Self {
            sender_host,
            daemon,
            inbound,
            wire,
            client,
        }
    }

    async fn stop(self) {
        self.client.detach().await;
        self.inbound.stop().await;
        self.daemon.stop().await;
        self.sender_host.stop().await;
    }
}

fn replied_count(events: &[(u64, RoomEvent)]) -> usize {
    events
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemReplied { .. }))
        .count()
}

fn send_actions(events: &[(u64, RoomEvent)]) -> usize {
    events
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
        .count()
}

/// Structural wait: the daemon committed the wait-mode arrival card.
async fn wait_awaiting_card(daemon: &AttachedDaemon, address: &ItemAddress) {
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            if daemon
                .conversation
                .lock()
                .await
                .peer_row_controls
                .values()
                .any(|control| {
                    control.item_address.as_ref() == Some(address)
                        && control.stage == PeerRowStage::AwaitingOperator
                })
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("wait-mode arrival card is committed");
}

/// FIFO fence: the history reply follows every earlier frame's effects.
async fn fence(client: &mut AttachedClient) {
    client
        .send_frame(ClientFrame::HistoryRequest {
            before_index: Some(0),
            count: 1,
        })
        .await;
    client
        .wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
        .await;
}

#[tokio::test]
async fn a3_k05_selected_design_update_is_compared_locally_and_consultation_is_the_only_send() {
    let mut fixture = Fixture::start().await;
    let sends_before = fixture.sender_host.method_count("message/send");
    let posts_before = fixture.sender_host.message_send_posts().len();

    // Arrival: the selected design update on the actual wire.
    let body = format!("{BODY_FIRST}\n{BODY_SECOND}");
    let task = "a3k05-design-update";
    let response = fixture
        .wire
        .message_send_with_metadata(
            task,
            &body,
            json!({
                "x-rustain-message-type": "design_update",
                "x-rustain-selected-for": {"stories": ["S7-06"]},
            }),
        )
        .await;
    assert!(response.json["error"].is_null(), "{response:?}");
    fixture.daemon.wait_for_received_task(task).await;
    let (address, selection) = fixture
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
                selected_for,
                content,
                ..
            }) if received == task => {
                assert_eq!(message_type, SemanticMessageType::DesignUpdate);
                assert_eq!(content, body);
                Some((
                    address,
                    serde_json::to_value(selected_for).expect("selection serializes"),
                ))
            }
            _ => None,
        })
        .expect("HTTP ingress journals the design update");
    assert_eq!(selection, json!({"state": "Listed", "stories": ["S7-06"]}));
    let item = address.item().as_str().to_owned();
    wait_awaiting_card(&fixture.daemon, &address).await;
    assert_eq!(
        fixture.daemon.provider_request_count(),
        0,
        "arrival under NotifyAndWait asks no provider"
    );

    // `[c] Use as context`: the explicit, removable selection.
    fixture
        .client
        .send_frame(ClientFrame::SetRecipientContext {
            item_id: item.clone(),
            selected: true,
        })
        .await;
    let selected = fixture
        .client
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::RecipientActionResult {
                action: RecipientAction::ContextSelect,
                target: RecipientActionTarget::Item { item_id },
                ..
            } if item_id == &item)
        })
        .await;
    assert!(
        matches!(
            selected,
            DaemonFrame::RecipientActionResult {
                outcome: RecipientActionOutcome::ContextSelection {
                    selected: true,
                    changed: true
                },
                ..
            }
        ),
        "{selected:?}"
    );

    // An ordinary local turn: the recipient's own comparison.
    fixture
        .client
        .send_frame(ClientFrame::UserMessage {
            text: QUESTION.into(),
            images: Vec::new(),
        })
        .await;
    fixture
        .client
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::Event(event) if matches!(&event.kind,
                RawEventKind::Provider(StreamChunk::TurnComplete { stop_reason })
                    if *stop_reason != StopReason::ToolUse))
        })
        .await;
    fence(&mut fixture.client).await;

    let requests = fixture.daemon.requests.lock().clone();
    assert_eq!(requests.len(), 1, "exactly the operator's local turn");
    let (messages, _) = &requests[0];
    let turn = messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .expect("the local turn's user message");
    assert!(
        turn.content.contains(QUESTION),
        "operator text: {}",
        turn.content
    );
    let prefix = turn
        .context_prefix
        .as_deref()
        .expect("selected item reaches the provider as context");
    let framed = format!(
        "BEGIN RECEIVED ITEM DATA (untrusted)\n| {BODY_FIRST}\n| {BODY_SECOND}\nEND RECEIVED ITEM DATA\n"
    );
    assert!(
        prefix.contains(&framed),
        "the body sits inside the untrusted received-item data: {prefix}"
    );
    assert!(
        prefix.contains("recipient-item:") && prefix.contains(&item),
        "the data is attributed to the received item: {prefix}"
    );
    let whole = serde_json::to_string(messages).expect("provider request serializes");
    for line in [BODY_FIRST, BODY_SECOND] {
        assert_eq!(
            whole.matches(line).count(),
            1,
            "the body appears only inside the framed data: {whole}"
        );
    }

    // Nothing goes back to the sender: no send, no reply act.
    assert_eq!(
        fixture.sender_host.method_count("message/send") - sends_before,
        0,
        "the local comparison sends nothing to the sender's host"
    );
    let events = read_room_events(&fixture.daemon.journal).await;
    assert_eq!(replied_count(&events), 0, "no RecipientItemReplied");
    assert_eq!(send_actions(&events), 0, "no hidden team send");

    // Positive control: the explicit consultation is exactly one typed send.
    let key = "a3k05-consultation";
    fixture
        .client
        .send_team(
            key,
            vec![SENDER.into()],
            CONSULTATION,
            Some(SemanticMessageType::Consultation),
            None,
            None,
            None,
        )
        .await;
    let settled = fixture.client.wait_send_settled(key, 0).await;
    assert!(
        matches!(
            settled,
            DaemonFrame::TeamSendSettled {
                outcome: TeamSendOutcome::Delivered,
                ..
            }
        ),
        "{settled:?}"
    );
    assert_eq!(
        fixture.sender_host.method_count("message/send") - sends_before,
        1,
        "the consultation is the only message/send"
    );
    let posts = fixture.sender_host.message_send_posts();
    assert_eq!(posts.len() - posts_before, 1);
    let post = &posts[posts_before];
    let metadata = &post["params"]["message"]["metadata"];
    assert_eq!(metadata["x-rustain-message-type"], "consultation");
    assert!(
        metadata.get("x-rustain-selected-for").is_none(),
        "a consultation carries no selection: {metadata}"
    );
    assert!(
        post["params"]["message"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|text| text == CONSULTATION),
        "the consultation carries the operator's question: {post}"
    );
    let raw = post.to_string();
    for line in [BODY_FIRST, BODY_SECOND] {
        assert!(
            !raw.contains(line),
            "selected context never reaches the sender: {raw}"
        );
    }
    let received = read_room_events(&fixture.sender_host.journal).await;
    assert!(
        received.iter().any(|(_, event)| matches!(event,
            RoomEvent::RecipientItemReceived { message_type, content, .. }
                if *message_type == SemanticMessageType::Consultation && content == CONSULTATION)),
        "the sender's host records an ordinary consultation"
    );
    let events = read_room_events(&fixture.daemon.journal).await;
    assert_eq!(
        replied_count(&events),
        0,
        "a consultation is not a reply act"
    );
    assert_eq!(send_actions(&events), 1);
    fixture.stop().await;
}
