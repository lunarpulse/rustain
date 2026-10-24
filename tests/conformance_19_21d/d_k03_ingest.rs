//! Story 19.21d K03: malformed carrier data is tolerated, never retained raw.
use super::attached_harness::{AttachedClient, AttachedDaemon, InboundSenderIdentity};
use super::harness::read_room_events;
use rustain::adapters::daemon::protocol::ConnectionTier;
use rustain::domain::models::{PeerId, ReceivedStories, RoomEvent, StoryHolder};
use serde_json::{Value, json};

const RAW_MARKER: &str = "K03-private-raw-carrier-marker";

async fn received(daemon: &AttachedDaemon, task: &str) -> Option<ReceivedStories> {
    daemon.wait_for_received_task(task).await;
    read_room_events(&daemon.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                task: found,
                stories,
                ..
            } if found == task => Some(stories),
            _ => None,
        })
        .expect("admitted HTTP task is durably recorded")
}

fn metadata(kind: &str, value: Value) -> Value {
    json!({"x-rustain-message-type":kind,"x-rustain-stories":value})
}

#[tokio::test]
async fn d_k03_http_tolerance_boundaries_and_consent_deferred_retention() {
    let daemon = AttachedDaemon::start(vec![]).await;
    let identity = InboundSenderIdentity::new("story-sender");
    let principal = identity.identity.peer_id.clone();
    let host = daemon.serve_http(vec![identity]).await;
    let sender = host.sender("story-sender");
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let pin = PeerId::from_public_key(&[7; 32]).unwrap();
    let alias_256 = "é".repeat(128);
    let readable = json!({"stories":[
        {"key":"S1","you":true,"future":RAW_MARKER},
        {"key":"S2","you":false},
        {"key":"S3","you":false,"alias":"member","peer":pin.as_str()},
        {"key":"S4","alias":alias_256},
    ],"future":RAW_MARKER});
    let pending = sender
        .message_send_with_metadata(
            "deferred-list",
            "deferred list body",
            metadata("story_assignment", readable.clone()),
        )
        .await;
    assert!(pending.json["error"].is_null(), "{pending:?}");
    assert_eq!(pending.task_state.as_deref(), Some("auth-required"));
    assert!(!read_room_events(&daemon.journal).await.iter().any(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { task, .. } if task == "deferred-list")));
    let (request, _) = writer.wait_sender_consent_request().await;
    writer.answer_sender_consent_always_and_save(request).await;
    daemon.wait_for_pinned_sender_consent(&principal).await;
    let deferred = received(&daemon, "deferred-list")
        .await
        .expect("consent launch retains IngressFacts list");
    let ReceivedStories::Listed { stories } = deferred else {
        panic!("readable deferred list expected")
    };
    assert_eq!(
        stories.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
        ["S1", "S2", "S3", "S4"]
    );
    assert_eq!(
        stories.into_iter().map(|s| s.holder).collect::<Vec<_>>(),
        vec![
            StoryHolder::You,
            StoryHolder::NoHolder,
            StoryHolder::Member {
                alias: "member".into(),
                peer: Some(pin)
            },
            StoryHolder::Member {
                alias: alias_256.clone(),
                peer: None
            },
        ]
    );
    for (task, carrier) in [
        (
            "absent",
            json!({"x-rustain-message-type":"story_assignment"}),
        ),
        ("wrong-type", metadata("design_update", readable.clone())),
        (
            "wrong-type-malformed",
            metadata("design_update", json!(RAW_MARKER)),
        ),
        ("untyped", json!({"x-rustain-stories":readable})),
    ] {
        let response = sender
            .message_send_with_metadata(task, "control body", carrier)
            .await;
        assert!(response.json["error"].is_null(), "{task}: {response:?}");
        assert_eq!(received(&daemon, task).await, None, "{task}");
    }
    let raw = std::fs::read_to_string(daemon.journal.node.path()).unwrap();
    assert!(
        !raw.contains(RAW_MARKER),
        "ignored raw fields are not journaled"
    );
    writer.detach().await;
    host.stop().await;
    daemon.stop().await;
    let mut invalid = vec![
        json!(null),
        json!(true),
        json!(42),
        json!(RAW_MARKER),
        json!([]),
        json!({}),
        json!({"stories":null}),
        json!({"stories":{}}),
        json!({"stories":[]}),
        json!({"stories":(0..65).map(|n| json!({"key":format!("S{n}")})).collect::<Vec<_>>()}),
        json!({"stories":[null]}),
        json!({"stories":["S1"]}),
        json!({"stories":[{}]}),
        json!({"stories":[{"key":1}]}),
        json!({"stories":[{"key":"bad!"}]}),
        json!({"stories":[{"key":""}]}),
        json!({"stories":[{"key":"a".repeat(33)}]}),
        json!({"stories":[{"key":"S1"},{"key":"S1"}]}),
        json!({"stories":[{"key":"S1","you":"true"}]}),
        json!({"stories":[{"key":"S1","you":null}]}),
        json!({"stories":[{"key":"S1","you":true,"alias":"member"}]}),
        json!({"stories":[{"key":"S1","you":true,"peer":principal.as_str()}]}),
        json!({"stories":[{"key":"S1","peer":principal.as_str()}]}),
        json!({"stories":[{"key":"S1","alias":42}]}),
        json!({"stories":[{"key":"S1","alias":null}]}),
        json!({"stories":[{"key":"S1","alias":""}]}),
        json!({"stories":[{"key":"S1","alias":format!("{alias_256}x")}]}),
        json!({"stories":[{"key":"S1","alias":"member","peer":42}]}),
        json!({"stories":[{"key":"S1","alias":"member","peer":null}]}),
        json!({"stories":[{"key":"S1","alias":"member","peer":"not-a-peer"}]}),
    ];
    // Each daemon's NodeTree permits ten root children. Keep each HTTP table
    // batch below that real production bound instead of bypassing admission.
    for (batch, values) in invalid.chunks_mut(8).enumerate() {
        let recipient = super::d_k04_render::StoryRecipient::start(
            rustain::domain::models::ResponseMode::NotifyAndWait,
        )
        .await;
        for (offset, value) in values.iter_mut().enumerate() {
            if let Some(object) = value.as_object_mut() {
                object.insert("private-extra".into(), json!(RAW_MARKER));
            }
            let index = batch * 8 + offset;
            let task = format!("invalid-carrier-{index}");
            recipient
                .send(
                    &task,
                    "malformed carrier still admitted",
                    Some(value.clone()),
                )
                .await;
            assert_eq!(
                received(&recipient.daemon, &task).await,
                Some(ReceivedStories::Unreadable),
                "case {index}: {value}"
            );
        }
        let raw = std::fs::read_to_string(recipient.daemon.journal.node.path()).unwrap();
        assert!(
            !raw.contains(RAW_MARKER),
            "raw values and ignored fields must not reach durable storage"
        );
        recipient.stop().await;
    }
}
