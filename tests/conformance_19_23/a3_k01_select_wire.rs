//! Story 19.23 K01: `--holders-of` selects through the lists this host really
//! received over HTTP and through its own statements; each recipient's wire
//! carrier and journal hold only the keys that selected it.
//!
//! Every leg enters through a real attached `TeamSend` frame on sender N and
//! real `message/send` POSTs into N and out to the recipient hosts. Nothing
//! here builds a selection or a received selection; the assertions read the
//! actual JSON-RPC posts and the actual recipient journals.
use std::sync::Arc;

use super::attached_harness::{
    AttachedClient, AttachedDaemon, InboundHttpSender, InboundSenderIdentity, RecipientHost,
};
use super::harness::{Journal, read_room_events};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{ConnectionTier, DaemonFrame, TeamSendOutcome};
use rustain::adapters::daemon::server::TeamSendTestHooks;
use rustain::domain::models::{
    A2aPeerSpec, IndividualPolicy, PeerId, PrincipalKind, ReceivedStories, RoomEvent,
    SemanticMessageType, StoryManifest, StorySelector,
};
use serde_json::{Value, json};

const SELECTED_FOR: &str = "x-rustain-selected-for";

/// Sender N: a NotifyAndWait daemon whose inbound senders already hold a
/// standing pinned grant, so received lists are journaled without a consent
/// card and nothing runs a model turn behind the test.
async fn start_sender_n(roster: Vec<A2aPeerSpec>, inbound: &[&PeerId]) -> AttachedDaemon {
    let journal = Journal::open().await;
    for peer in inbound {
        journal
            .room
            .record_event(RoomEvent::ConsentGranted {
                sender: Some((*peer).clone()),
                granted_at: 1_800_000_000_000,
                principal: Some(PrincipalKind::Pinned),
            })
            .await
            .expect("persist standing inbound sender consent");
    }
    let policy = Arc::new(
        rustain::domain::services::team_policy::resolve_effective_policy(
            &IndividualPolicy::default(),
            None,
            &roster,
        ),
    );
    let room = journal.room.clone();
    let daemon = AttachedDaemon::compose_with_clock_and_test_hooks_and_policy(
        journal,
        roster,
        room,
        Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
            1_800_000_000_000,
        )),
        TeamSendTestHooks::new(None, None, false),
        Some(policy),
    )
    .await;
    for peer in inbound {
        daemon.wait_for_pinned_sender_consent(peer).await;
    }
    daemon
}

/// One real inbound `message/send` carrying a story list; returns once the
/// HTTP answer is back (after the production launch) and the item is durable
/// with a readable list.
async fn receive_list(
    daemon: &AttachedDaemon,
    sender: &InboundHttpSender,
    task: &str,
    kind: &str,
    stories: Value,
) {
    let result = sender
        .message_send_with_metadata(
            task,
            &format!("{task} list body"),
            json!({"x-rustain-message-type": kind, "x-rustain-stories": {"stories": stories}}),
        )
        .await;
    assert!(result.json["error"].is_null(), "{task}: {result:?}");
    daemon.wait_for_received_task(task).await;
    assert!(
        read_room_events(&daemon.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(
                event,
                RoomEvent::RecipientItemReceived {
                    task: received,
                    stories: Some(ReceivedStories::Listed { .. }),
                    ..
                } if received == task
            )),
        "{task}: the fixture list must be journaled as readable on N"
    );
}

fn pinned_holder(key: &str, display_alias: &str, pin: &PeerId) -> Value {
    json!({"key": key, "alias": display_alias, "peer": pin.as_str()})
}

fn pin_of(host: &RecipientHost, alias: &str) -> PeerId {
    host.pinned_peer(alias)
        .pinned_identity()
        .expect("actual recipient signer pin")
}

/// Wait for the request's start and every recipient's Delivered settlement;
/// return the action id the daemon reported.
async fn delivered(writer: &mut AttachedClient, request: &str, recipients: usize) -> String {
    let started = writer.wait_send_started(request).await;
    let DaemonFrame::TeamSendStarted { action, .. } = started else {
        panic!("{request}: expected TeamSendStarted, got {started:?}")
    };
    for index in 0..recipients {
        let frame = writer.wait_send_settled(request, index).await;
        assert!(
            matches!(
                frame,
                DaemonFrame::TeamSendSettled {
                    outcome: TeamSendOutcome::Delivered,
                    ..
                }
            ),
            "{request}[{index}]: {frame:?}"
        );
    }
    action.as_str().to_owned()
}

/// The recorded action for `action`: (recipient aliases in order, whether it
/// recorded any story list, its type).
async fn action_record(
    daemon: &AttachedDaemon,
    action: &str,
) -> (Vec<String>, bool, SemanticMessageType) {
    read_room_events(&daemon.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::TeamSendActionRecorded {
                action: recorded,
                recipients,
                stories,
                message_type,
                ..
            } if recorded.as_str() == action => Some((
                recipients.into_iter().map(|r| r.alias).collect(),
                !stories.is_empty(),
                message_type,
            )),
            _ => None,
        })
        .expect("the started action is durably recorded on N")
}

async fn action_count(daemon: &AttachedDaemon) -> usize {
    read_room_events(&daemon.journal)
        .await
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
        .count()
}

/// The single actual JSON-RPC post whose body is `body`.
fn post_for(host: &RecipientHost, body: &str) -> Value {
    let posts = host
        .message_send_posts()
        .into_iter()
        .filter(|post| post["params"]["message"]["parts"][0]["text"] == body)
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 1, "exactly one message/send carries {body:?}");
    posts.into_iter().next().unwrap()
}

/// The recipient journal's actual stored selection for `body`, serialized.
async fn received_selection(host: &RecipientHost, body: &str) -> (Value, bool) {
    read_room_events(&host.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                content,
                selected_for,
                stories,
                ..
            } if content == body => Some((
                serde_json::to_value(selected_for).unwrap(),
                stories.is_some(),
            )),
            _ => None,
        })
        .expect("the real recipient journaled the item")
}

#[tokio::test]
async fn a3_k01_holders_of_reaches_each_pinned_holder_with_only_its_own_keys() {
    let jun = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let aisha = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let marcus = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let jun_pin = pin_of(&jun, "jun");
    let aisha_pin = pin_of(&aisha, "aisha");
    let lead = InboundSenderIdentity::new("lead-l");
    let lead_peer = lead.identity.peer_id.clone();
    let daemon = start_sender_n(
        vec![
            jun.pinned_peer("jun"),
            aisha.pinned_peer("aisha"),
            marcus.pinned_peer("marcus"),
        ],
        &[&lead_peer],
    )
    .await;
    let inbound = daemon.serve_http(vec![lead]).await;
    let l = inbound.sender("lead-l");
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    // L's real story_assignment names the holders by pin; its display aliases
    // are L's own names, never N's roster aliases.
    receive_list(
        &daemon,
        &l,
        "l-assignment",
        "story_assignment",
        json!([
            pinned_holder("S7-04", "jun-as-l-calls-them", &jun_pin),
            pinned_holder("S7-06", "aisha-as-l-calls-them", &aisha_pin),
        ]),
    )
    .await;

    // ── Selection: one action, jun then aisha, one POST each, own keys only.
    let hosts = [&jun, &aisha, &marcus];
    let before_posts = hosts.map(|h| h.method_count("message/send"));
    let before_actions = action_count(&daemon).await;
    let body = "a3-k01 payment UI design update";
    writer
        .send_team(
            "a3-k01-select",
            Vec::new(),
            body,
            Some(SemanticMessageType::DesignUpdate),
            None,
            None,
            Some(StorySelector::parse("S7-04,S7-06").unwrap()),
        )
        .await;
    let action = delivered(&mut writer, "a3-k01-select", 2).await;
    let started = writer
        .frames_for_request("a3-k01-select")
        .into_iter()
        .find(|frame| matches!(frame, DaemonFrame::TeamSendStarted { .. }))
        .unwrap();
    assert_eq!(
        serde_json::to_value(started).unwrap()["teamSendStarted"]["recipients"],
        json!(["jun", "aisha"])
    );
    assert_eq!(action_count(&daemon).await, before_actions + 1);
    let (recipients, recorded_list, kind) = action_record(&daemon, &action).await;
    assert_eq!(
        recipients,
        ["jun", "aisha"],
        "first-key order, roster aliases"
    );
    assert!(
        !recorded_list,
        "a selection is never recorded as an assignment"
    );
    assert_eq!(kind, SemanticMessageType::DesignUpdate);
    assert_eq!(
        hosts.map(|h| h.method_count("message/send")),
        [before_posts[0] + 1, before_posts[1] + 1, before_posts[2]],
        "exactly one message/send per selected holder, none elsewhere"
    );
    for (host, keys) in [(&jun, json!(["S7-04"])), (&aisha, json!(["S7-06"]))] {
        let metadata = &post_for(host, body)["params"]["message"]["metadata"];
        assert_eq!(metadata[SELECTED_FOR], json!({"stories": keys}));
        assert_eq!(metadata["x-rustain-message-type"], "design_update");
        assert!(
            metadata.get("x-rustain-stories").is_none(),
            "a selection never rides the story-list carrier: {metadata}"
        );
        let (selected, listed) = received_selection(host, body).await;
        assert_eq!(selected, json!({"state": "Listed", "stories": keys}));
        assert!(
            !listed,
            "the recipient records no story list for a selection"
        );
    }

    // ── Own-statement leg (OR-1a): N's own assignment selects marcus.
    writer
        .send_team(
            "a3-k01-own-assign",
            vec!["marcus".into()],
            "a3-k01 S7-09 is yours",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(StoryManifest::parse("S7-09=marcus").unwrap()),
            None,
        )
        .await;
    delivered(&mut writer, "a3-k01-own-assign", 1).await;
    let before_posts = hosts.map(|h| h.method_count("message/send"));
    let own_body = "a3-k01 S7-09 design update";
    writer
        .send_team(
            "a3-k01-own-select",
            Vec::new(),
            own_body,
            Some(SemanticMessageType::DesignUpdate),
            None,
            None,
            Some(StorySelector::parse("S7-09").unwrap()),
        )
        .await;
    let action = delivered(&mut writer, "a3-k01-own-select", 1).await;
    assert_eq!(action_record(&daemon, &action).await.0, ["marcus"]);
    assert_eq!(
        hosts.map(|h| h.method_count("message/send")),
        [before_posts[0], before_posts[1], before_posts[2] + 1]
    );
    assert_eq!(
        post_for(&marcus, own_body)["params"]["message"]["metadata"][SELECTED_FOR],
        json!({"stories": ["S7-09"]})
    );
    assert_eq!(
        received_selection(&marcus, own_body).await.0,
        json!({"state": "Listed", "stories": ["S7-09"]})
    );

    // ── Control: a plain design_update carries no selection and stores none.
    let plain_body = "a3-k01 plain design update";
    writer
        .send_team(
            "a3-k01-plain",
            vec!["jun".into()],
            plain_body,
            Some(SemanticMessageType::DesignUpdate),
            None,
            None,
            None,
        )
        .await;
    delivered(&mut writer, "a3-k01-plain", 1).await;
    let started = writer
        .frames_for_request("a3-k01-plain")
        .into_iter()
        .find(|frame| matches!(frame, DaemonFrame::TeamSendStarted { .. }))
        .unwrap();
    assert!(
        serde_json::to_value(started).unwrap()["teamSendStarted"]
            .get("recipients")
            .is_none()
    );
    let metadata = &post_for(&jun, plain_body)["params"]["message"]["metadata"];
    assert!(
        metadata.get(SELECTED_FOR).is_none(),
        "no selection key without --holders-of: {metadata}"
    );
    assert_eq!(received_selection(&jun, plain_body).await.0, Value::Null);

    writer.detach().await;
    inbound.stop().await;
    daemon.stop().await;
    jun.stop().await;
    aisha.stop().await;
    marcus.stop().await;
}
