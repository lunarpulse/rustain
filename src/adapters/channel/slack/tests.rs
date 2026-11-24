//! Slack adapter behaviour over the typed local transport (Story C-12 AC3-AC4).

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;

use super::fixture::{ConnectStep, FakeSlack, socket};
use super::*;
use crate::domain::models::HealthLevel;

const ALICE: &str = "U111AAAA";
const BOB: &str = "U222BBBB";
const DM_ALICE: &str = "D0ALICE01";
const DM_BOB: &str = "D0BOB0001";

fn hello() -> SocketEvent {
    SocketEvent::Text(json!({"type": "hello", "num_connections": 1}).to_string())
}

fn dm(envelope: &str, channel: &str, user: &str, text: &str, ts: &str) -> SocketEvent {
    SocketEvent::Text(
        json!({
            "envelope_id": envelope,
            "type": "events_api",
            "payload": {
                "type": "event_callback",
                "authorizations": [{"user_id": "UBOT0001", "is_bot": true}],
                "event": {"type": "message", "channel_type": "im", "channel": channel,
                          "user": user, "text": text, "ts": ts},
            },
        })
        .to_string(),
    )
}

struct Harness {
    adapter: SlackChannelAdapter,
    fake: Arc<FakeSlack>,
    turns: mpsc::UnboundedReceiver<ChannelTurnRequest>,
}

fn harness(steps: Vec<ConnectStep>, allowed: &[&str]) -> Harness {
    let fake = FakeSlack::new(steps);
    let (turn_tx, turns) = mpsc::unbounded_channel();
    let adapter = SlackChannelAdapter::with_transport(
        fake.clone(),
        allowed.iter().map(|id| id.to_string()).collect(),
        turn_tx,
    )
    .expect("composes");
    Harness {
        adapter,
        fake,
        turns,
    }
}

async fn next_state(rx: &mut watch::Receiver<ChannelLinkState>, want: ChannelLinkState) {
    tokio::time::timeout(
        Duration::from_secs(600),
        rx.wait_for(|state| *state == want),
    )
    .await
    .expect("state reached")
    .expect("sender alive");
}

#[tokio::test(start_paused = true)]
async fn allowed_dm_is_acked_dispatched_as_slack_origin_and_answered_in_the_same_dm() {
    let mut h = harness(vec![], &[ALICE, BOB]);
    let (frames, step) = socket();
    h.fake.push(step);
    let mut link = h.adapter.health_updates().expect("publishes link state");
    assert_eq!(*link.borrow(), ChannelLinkState::Connecting);
    h.adapter.start_loop().await.unwrap();
    frames.send(hello()).unwrap();
    next_state(&mut link, ChannelLinkState::Connected).await;
    assert_eq!(h.adapter.health_snapshot().level, HealthLevel::Healthy);

    frames
        .send(dm("env-1", DM_ALICE, ALICE, "what is 6*7?", "1.0001"))
        .unwrap();
    let request = h.turns.recv().await.expect("dispatched");
    assert_eq!(request.text, "what is 6*7?");
    assert_eq!(request.origin, ChannelKind::Slack);
    request.response_tx.send("42".into()).unwrap();
    h.fake.posted.acquire().await.unwrap().forget();

    assert_eq!(h.fake.acks(), [r#"{"envelope_id":"env-1"}"#]);
    assert_eq!(h.fake.posts(), [(DM_ALICE.to_string(), "42".to_string())]);
    h.adapter.shutdown_loop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn denied_senders_rooms_bots_and_malformed_frames_are_acked_but_never_dispatched() {
    let mut h = harness(vec![], &[ALICE]);
    let (frames, step) = socket();
    h.fake.push(step);
    h.adapter.start_loop().await.unwrap();
    frames.send(hello()).unwrap();
    let raw = |envelope: &str, event: serde_json::Value| {
        SocketEvent::Text(
            json!({"envelope_id": envelope, "type": "events_api",
                   "payload": {"type": "event_callback", "event": event}})
            .to_string(),
        )
    };
    frames
        .send(dm("env-deny", DM_BOB, "U999ZZZZ", "let me in", "2.1"))
        .unwrap();
    frames
        .send(raw(
            "env-public",
            json!({"type":"message","channel_type":"channel","channel":"C0PUBLIC1",
                   "user":ALICE,"text":"hi","ts":"2.2"}),
        ))
        .unwrap();
    frames
        .send(raw(
            "env-bot",
            json!({"type":"message","channel_type":"im","channel":DM_ALICE,
                   "user":ALICE,"bot_id":"B1","text":"hi","ts":"2.3"}),
        ))
        .unwrap();
    frames.send(SocketEvent::Text("{not json".into())).unwrap();
    frames
        .send(dm("env-empty", DM_ALICE, ALICE, " \u{7} ", "2.4"))
        .unwrap();
    // The one admitted DM goes last: when it dispatches, every earlier frame has
    // already been handled in order, so "nothing before it dispatched" is exact.
    frames
        .send(dm("env-ok", DM_ALICE, ALICE, "real question", "2.5"))
        .unwrap();
    let request = h.turns.recv().await.expect("the allowed DM dispatches");
    assert_eq!(request.text, "real question");
    assert!(h.turns.try_recv().is_err(), "nothing else was dispatched");
    request.response_tx.send("ok".into()).unwrap();
    h.fake.posted.acquire().await.unwrap().forget();
    assert_eq!(
        h.fake.posts(),
        [(DM_ALICE.to_string(), "ok".to_string())],
        "no Slack reply to any denied message"
    );
    let acks = h.fake.acks();
    for envelope in ["env-deny", "env-public", "env-bot", "env-empty", "env-ok"] {
        assert!(
            acks.contains(&format!(r#"{{"envelope_id":"{envelope}"}}"#)),
            "{envelope} acknowledged"
        );
    }
    assert_eq!(acks.len(), 5, "the malformed frame has no envelope to ack");
    h.adapter.shutdown_loop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn each_reply_goes_to_the_dm_it_came_from_not_another_allowed_users_dm() {
    let mut h = harness(vec![], &[ALICE, BOB]);
    let (frames, step) = socket();
    h.fake.push(step);
    h.adapter.start_loop().await.unwrap();
    frames.send(hello()).unwrap();
    frames
        .send(dm("env-a", DM_ALICE, ALICE, "from alice", "3.1"))
        .unwrap();
    frames
        .send(dm("env-b", DM_BOB, BOB, "from bob", "3.2"))
        .unwrap();
    for (expected, reply) in [("from alice", "to alice"), ("from bob", "to bob")] {
        let request = h.turns.recv().await.expect("dispatched in order");
        assert_eq!(request.text, expected);
        request.response_tx.send(reply.into()).unwrap();
        h.fake.posted.acquire().await.unwrap().forget();
    }
    assert_eq!(
        h.fake.posts(),
        [
            (DM_ALICE.to_string(), "to alice".to_string()),
            (DM_BOB.to_string(), "to bob".to_string())
        ]
    );
    h.adapter.shutdown_loop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_replayed_envelope_is_acked_and_answered_once() {
    let mut h = harness(vec![], &[ALICE]);
    let (frames, step) = socket();
    h.fake.push(step);
    h.adapter.start_loop().await.unwrap();
    frames.send(hello()).unwrap();
    frames
        .send(dm("env-1", DM_ALICE, ALICE, "once", "4.1"))
        .unwrap();
    frames
        .send(dm("env-1", DM_ALICE, ALICE, "once", "4.1"))
        .unwrap();
    frames
        .send(dm("env-2", DM_ALICE, ALICE, "twice", "4.2"))
        .unwrap();
    let first = h.turns.recv().await.unwrap();
    assert_eq!(first.text, "once");
    first.response_tx.send("r1".into()).unwrap();
    let second = h.turns.recv().await.unwrap();
    assert_eq!(second.text, "twice", "the replay produced no turn");
    second.response_tx.send("r2".into()).unwrap();
    h.fake.posted.acquire_many(2).await.unwrap().forget();
    assert_eq!(h.fake.acks().len(), 3, "the replay was still acknowledged");
    h.adapter.shutdown_loop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn long_replies_post_as_ordered_chunks_and_a_failed_chunk_stops_the_rest() {
    let mut h = harness(vec![], &[ALICE]);
    let (frames, step) = socket();
    h.fake.push(step);
    h.adapter.start_loop().await.unwrap();
    frames.send(hello()).unwrap();
    frames
        .send(dm("env-1", DM_ALICE, ALICE, "go", "5.1"))
        .unwrap();
    let request = h.turns.recv().await.unwrap();
    let reply = format!("{}\n{}", "a".repeat(2000), "b".repeat(2000));
    request.response_tx.send(reply).unwrap();
    h.fake.posted.acquire_many(2).await.unwrap().forget();
    let posts = h.fake.posts();
    assert_eq!(posts.len(), 2);
    assert!(posts[0].1.starts_with('a') && posts[1].1.starts_with('b'));
    assert!(posts.iter().all(|(channel, _)| channel == DM_ALICE));

    // A failing first chunk: the second is never attempted (no retry, no duplicate).
    h.fake
        .fail_posts
        .store(true, std::sync::atomic::Ordering::SeqCst);
    frames
        .send(dm("env-2", DM_ALICE, ALICE, "again", "5.2"))
        .unwrap();
    let request = h.turns.recv().await.unwrap();
    request
        .response_tx
        .send(format!("{}\n{}", "c".repeat(2000), "d".repeat(2000)))
        .unwrap();
    h.fake.posted.acquire().await.unwrap().forget();
    assert_eq!(h.fake.posts().len(), 2, "nothing new was recorded");
    assert!(h.fake.posted.try_acquire().is_err(), "exactly one attempt");
    h.adapter.shutdown_loop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn pending_turns_are_bounded() {
    let mut h = harness(vec![], &[ALICE]);
    let (frames, step) = socket();
    h.fake.push(step);
    h.adapter.start_loop().await.unwrap();
    frames.send(hello()).unwrap();
    let total = PENDING_TURN_CAPACITY + 6;
    for n in 0..total {
        frames
            .send(dm(
                &format!("env-{n}"),
                DM_ALICE,
                ALICE,
                &format!("m{n}"),
                &format!("6.{n}"),
            ))
            .unwrap();
    }
    // Every frame is already queued, so the connection loop admits all of them
    // before the reply worker first runs: the bound, not scheduling, decides.
    drop(frames);
    for _ in 0..PENDING_TURN_CAPACITY {
        let request = h.turns.recv().await.expect("a queued DM dispatches");
        request.response_tx.send("ok".into()).unwrap();
        h.fake.posted.acquire().await.unwrap().forget();
    }
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        h.turns.try_recv().is_err(),
        "DMs beyond the bounded queue were dropped, not queued"
    );
    assert_eq!(
        h.fake.acks().len(),
        total,
        "every envelope was still acknowledged"
    );
    h.adapter.shutdown_loop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn repeated_failure_makes_exactly_five_retries_with_exponential_backoff_then_offline() {
    let h = harness(vec![], &[ALICE]);
    let mut link = h.adapter.health_updates().unwrap();
    h.adapter.start_loop().await.unwrap();
    next_state(&mut link, ChannelLinkState::Offline).await;
    assert_eq!(h.fake.connect_count(), 1 + MAX_RECONNECT_RETRIES as usize);
    let times = h.fake.connect_times.lock().clone();
    let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
    assert_eq!(gaps, [1, 2, 4, 8, 16]);
    assert_eq!(h.adapter.health_snapshot().level, HealthLevel::Error);
    assert_eq!(h.adapter.health_snapshot().metric, "offline");
    // No sixth retry, however long we wait.
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(h.fake.connect_count(), 6);
    h.adapter.shutdown_loop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_successful_reconnect_resets_the_retry_budget() {
    // open+hello, lost; two failed retries; a retry that reaches hello (budget
    // reset); lost again; then every retry fails: five more, then offline.
    let (first_tx, first) = socket();
    let (second_tx, second) = socket();
    first_tx.send(hello()).unwrap();
    second_tx.send(hello()).unwrap();
    let h = harness(
        vec![first, ConnectStep::Fail, ConnectStep::Fail, second],
        &[ALICE],
    );
    let mut link = h.adapter.health_updates().unwrap();
    h.adapter.start_loop().await.unwrap();
    next_state(&mut link, ChannelLinkState::Connected).await;
    drop(first_tx);
    next_state(&mut link, ChannelLinkState::Reconnecting).await;
    next_state(&mut link, ChannelLinkState::Connected).await;
    drop(second_tx);
    next_state(&mut link, ChannelLinkState::Offline).await;
    assert_eq!(
        h.fake.connect_count(),
        1 + 3 + MAX_RECONNECT_RETRIES as usize,
        "without the reset the budget would have run out after six attempts"
    );
    h.adapter.shutdown_loop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_server_disconnect_request_reconnects_without_spending_the_budget() {
    let (first_tx, first) = socket();
    let (second_tx, second) = socket();
    first_tx.send(hello()).unwrap();
    second_tx.send(hello()).unwrap();
    let h = harness(vec![first, second], &[ALICE]);
    let mut link = h.adapter.health_updates().unwrap();
    h.adapter.start_loop().await.unwrap();
    next_state(&mut link, ChannelLinkState::Connected).await;
    first_tx
        .send(SocketEvent::Text(
            json!({"type": "disconnect", "reason": "refresh_requested"}).to_string(),
        ))
        .unwrap();
    next_state(&mut link, ChannelLinkState::Reconnecting).await;
    next_state(&mut link, ChannelLinkState::Connected).await;
    assert_eq!(h.fake.connect_count(), 2);
    h.adapter.shutdown_loop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_cancels_a_pending_reconnect_and_a_waiting_turn_and_joins() {
    // Pending reconnect: the first attempt fails and the loop sleeps.
    let h = harness(vec![], &[ALICE]);
    h.adapter.start_loop().await.unwrap();
    tokio::task::yield_now().await;
    h.adapter.shutdown_loop().await.unwrap();
    let attempts = h.fake.connect_count();
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(
        h.fake.connect_count(),
        attempts,
        "no attempt after shutdown"
    );

    // A waiting turn: nobody answers; nothing is posted after shutdown.
    let mut h = harness(vec![], &[ALICE]);
    let (frames, step) = socket();
    h.fake.push(step);
    h.adapter.start_loop().await.unwrap();
    frames.send(hello()).unwrap();
    frames
        .send(dm("env-1", DM_ALICE, ALICE, "hang", "7.1"))
        .unwrap();
    let request = h.turns.recv().await.unwrap();
    h.adapter.shutdown_loop().await.unwrap();
    let _ = request.response_tx.send("too late".into());
    tokio::task::yield_now().await;
    assert!(h.fake.posts().is_empty(), "no send after cancellation");
}

#[test]
fn composition_rejects_unusable_allowlists_and_never_echoes_values() {
    let (turn_tx, _rx) = mpsc::unbounded_channel();
    for (allowed, needle) in [
        (vec![], "at least one"),
        (vec!["C0AAAA111".to_string()], "only Slack user IDs"),
        (vec!["u111aaaa".to_string()], "only Slack user IDs"),
        (vec!["*".to_string()], "only Slack user IDs"),
    ] {
        let sentinel = allowed.first().cloned().unwrap_or_default();
        let error =
            SlackChannelAdapter::with_transport(FakeSlack::new(vec![]), allowed, turn_tx.clone())
                .err()
                .expect("rejected")
                .to_string();
        assert!(error.contains(needle), "{error}");
        if !sentinel.is_empty() {
            assert!(!error.contains(&sentinel), "value echoed: {error}");
        }
    }
}

fn config(toml_text: &str) -> toml::Value {
    toml_text.parse().unwrap()
}

#[test]
fn compose_names_only_the_missing_requirement_and_never_the_secret() {
    let (turn_tx, _rx) = mpsc::unbounded_channel();
    let good = config(r#"allowed_user_ids = ["U111AAAA"]"#);
    let secret = "xoxb-secret-sentinel";
    let cases: Vec<(Option<&toml::Value>, Option<String>, Option<String>, &str)> = vec![
        (
            None,
            Some("a".into()),
            Some("b".into()),
            "allowed_user_ids required",
        ),
        (
            Some(&good),
            None,
            Some(secret.into()),
            "SLACK_APP_TOKEN env var required",
        ),
        (
            Some(&good),
            Some("   ".into()),
            Some(secret.into()),
            "SLACK_APP_TOKEN env var required",
        ),
        (
            Some(&good),
            Some("xapp-1".into()),
            None,
            "SLACK_BOT_TOKEN env var required",
        ),
    ];
    for (conf, app, bot, needle) in cases {
        let error = compose(conf, app, bot, Some(turn_tx.clone()))
            .err()
            .expect("rejected")
            .to_string();
        assert!(error.contains(needle), "{error}");
        assert!(!error.contains(secret), "{error}");
    }
    let wrong_kind = config("allowed_user_ids = [123456]");
    let error = compose(
        Some(&wrong_kind),
        Some("a".into()),
        Some("b".into()),
        Some(turn_tx.clone()),
    )
    .err()
    .expect("rejected")
    .to_string();
    assert!(error.contains("only Slack user ID strings"), "{error}");
    let empty = config("allowed_user_ids = []");
    assert!(
        compose(
            Some(&empty),
            Some("a".into()),
            Some("b".into()),
            Some(turn_tx.clone())
        )
        .is_err()
    );
    let error = compose(Some(&good), Some("a".into()), Some("b".into()), None)
        .err()
        .expect("daemon-only")
        .to_string();
    assert!(error.contains("daemon-only"), "{error}");
    let adapter = compose(
        Some(&good),
        Some("a".into()),
        Some("b".into()),
        Some(turn_tx),
    )
    .expect("composes with every requirement");
    assert_eq!(adapter.health_snapshot().metric, "connecting");
    assert!(adapter.health_updates().is_some());
}

#[test]
fn a_slack_adapter_never_posts_unattended_notices() {
    let (turn_tx, _rx) = mpsc::unbounded_channel();
    let fake = FakeSlack::new(vec![]);
    let adapter =
        SlackChannelAdapter::with_transport(fake.clone(), vec![ALICE.into()], turn_tx).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime
        .block_on(adapter.notify("cron result (forward = true)"))
        .unwrap();
    assert!(
        fake.posts().is_empty(),
        "notify is the trait's no-op for Slack"
    );
}

#[test]
fn backoff_schedule_is_exponential_and_capped() {
    assert_eq!(
        (1..=7).map(backoff_secs).collect::<Vec<_>>(),
        [1, 2, 4, 8, 16, 30, 30]
    );
}
