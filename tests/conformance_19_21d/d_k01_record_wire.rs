//! Story 19.21d K01: one frozen sender record, two real recipient carriers.
use super::attached_harness::{AppendGateJournal, AttachedClient, AttachedDaemon, RecipientHost};
use super::harness::{Journal, read_room_events};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{ConnectionTier, DaemonFrame, TeamSendOutcome};
use rustain::domain::models::{
    ReceivedStories, RoomEvent, SemanticMessageType, StoryHolder, StoryManifest,
};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn d_k01_frozen_record_precedes_io_and_whole_list_reaches_each_host() {
    let b = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let c = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let c_spec = c.pinned_peer("c");
    let c_pin = c_spec
        .pinned_identity()
        .expect("actual recipient signer pin");
    let journal = Journal::open().await;
    let gate = Arc::new(AppendGateJournal::new(
        journal.room.clone(),
        |event| matches!(event, RoomEvent::TeamSendActionRecorded { .. }),
        true,
    ));
    let daemon =
        AttachedDaemon::start_with_journal(journal, vec![b.peer("b"), c_spec], Some(gate.clone()))
            .await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let manifest = StoryManifest::parse("S1,S2=b,S3=c").unwrap();
    manifest
        .check(
            Some(SemanticMessageType::StoryAssignment),
            &["b".into(), "c".into()],
        )
        .unwrap();
    writer
        .send_team(
            "listed",
            vec!["b".into(), "c".into()],
            "listed body",
            Some(SemanticMessageType::StoryAssignment),
            None,
            Some(manifest),
        )
        .await;
    gate.wait_for(1).await;
    assert_eq!(
        b.method_count("message/send"),
        0,
        "no recipient I/O before durable action append"
    );
    assert_eq!(c.method_count("message/send"), 0);
    assert!(gate.durable_seen().is_empty());
    gate.release_append();
    for index in 0..2 {
        assert!(matches!(
            writer.wait_send_settled("listed", index).await,
            DaemonFrame::TeamSendSettled {
                outcome: TeamSendOutcome::Delivered,
                ..
            }
        ));
    }
    let events = read_room_events(&daemon.journal).await;
    let (action_seq, recorded) = events
        .iter()
        .find_map(|(seq, event)| match event {
            RoomEvent::TeamSendActionRecorded {
                stories,
                recipients,
                ..
            } => {
                assert_eq!(
                    recipients
                        .iter()
                        .map(|r| r.alias.as_str())
                        .collect::<Vec<_>>(),
                    ["b", "c"]
                );
                Some((*seq, stories))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        serde_json::to_value(recorded).unwrap(),
        json!([{"key":"S1","to":null},{"key":"S2","to":0},{"key":"S3","to":1}])
    );
    for (seq, event) in &events {
        if matches!(event, RoomEvent::RemoteEnvelopeDispatched { .. }) {
            assert!(*seq > action_seq);
        }
    }
    for (host, expected, holders) in [
        (
            &b,
            json!({"stories":[{"key":"S1"},{"key":"S2","you":true},{"key":"S3","alias":"c","peer":c_pin.as_str()}]}),
            vec![
                StoryHolder::NoHolder,
                StoryHolder::You,
                StoryHolder::Member {
                    alias: "c".into(),
                    peer: Some(c_pin.clone()),
                },
            ],
        ),
        (
            &c,
            json!({"stories":[{"key":"S1"},{"key":"S2","alias":"b"},{"key":"S3","you":true}]}),
            vec![
                StoryHolder::NoHolder,
                StoryHolder::Member {
                    alias: "b".into(),
                    peer: None,
                },
                StoryHolder::You,
            ],
        ),
    ] {
        assert_eq!(
            host.message_send_posts()[0]["params"]["message"]["metadata"]["x-rustain-stories"],
            expected
        );
        let received = read_room_events(&host.journal)
            .await
            .into_iter()
            .find_map(|(_, event)| match event {
                RoomEvent::RecipientItemReceived {
                    stories, content, ..
                } if content == "listed body" => stories,
                _ => None,
            })
            .expect("real recipient journal has a typed list");
        let ReceivedStories::Listed { stories } = received else {
            panic!("readable carrier expected")
        };
        assert_eq!(
            stories.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
            ["S1", "S2", "S3"]
        );
        assert_eq!(
            stories.into_iter().map(|s| s.holder).collect::<Vec<_>>(),
            holders
        );
    }
    writer
        .send_team(
            "legacy",
            vec!["b".into(), "c".into()],
            "legacy body",
            Some(SemanticMessageType::StoryAssignment),
            None,
            None,
        )
        .await;
    gate.wait_for(2).await;
    gate.release_append();
    for index in 0..2 {
        assert!(matches!(
            writer.wait_send_settled("legacy", index).await,
            DaemonFrame::TeamSendSettled {
                outcome: TeamSendOutcome::Delivered,
                ..
            }
        ));
    }
    for host in [&b, &c] {
        let posts = host.message_send_posts();
        assert_eq!(posts.len(), 2);
        assert!(
            posts[1]["params"]["message"]["metadata"]
                .get("x-rustain-stories")
                .is_none()
        );
        let raw = std::fs::read_to_string(host.journal.node.path()).unwrap();
        let event = raw
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|line| {
                line["record"]["payload"]["event"] == "recipient_item_received"
                    && line["record"]["payload"]["content"] == "legacy body"
            })
            .expect("raw legacy recipient line");
        assert!(
            event["record"]["payload"].get("stories").is_none(),
            "None must be absent, not null"
        );
    }
    let raw = std::fs::read_to_string(daemon.journal.node.path()).unwrap();
    let legacy = raw
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .rfind(|line| line["record"]["payload"]["event"] == "team_send_action_recorded")
        .unwrap();
    assert!(legacy["record"]["payload"].get("stories").is_none());
    writer.detach().await;
    daemon.stop().await;
    b.stop().await;
    c.stop().await;
}
