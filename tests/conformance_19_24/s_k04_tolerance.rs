//! Story 19.24: distrust raw served status, and scope reports to their poll item.
//! Legacy literal-journal compatibility is covered by the domain fold units;
//! these keystones deliberately enter through HTTP and the command front door.
use super::attached_harness::{AttachedClient, RecipientHost};
use super::harness::{Sender, folded_actions, read_room_events};
use super::k02_board_rules::{ControlledPeer, controlled_pinned_peer};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::send::{ThreadRead, read_thread};
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionOutcome,
    RecipientActionTarget,
};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{A2aPeerSpec, ReplyKind, RoomEvent};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RAW_STATUS_CANARY: &str = "RAW_STATUS_BODY_MUST_NEVER_REACH_THE_REPORT";

async fn mount_result(
    server: &MockServer,
    rpc_method: &str,
    result: Arc<parking_lot::Mutex<Value>>,
) {
    Mock::given(method("POST"))
        .and(path("/jsonrpc"))
        .and(body_partial_json(json!({"method": rpc_method})))
        .respond_with(move |request: &wiremock::Request| {
            let input: Value = serde_json::from_slice(&request.body).expect("JSON-RPC request");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": input["id"], "result": result.lock().clone()
            }))
        })
        .with_priority(1)
        .mount(server)
        .await;
}

async fn rpc_requests(server: &MockServer, rpc_method: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("recorded HTTP requests")
        .into_iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|request| request["method"] == rpc_method)
        .collect()
}

fn status_thread(item: &str, task: &str, status: Value) -> Value {
    json!({
        "root": item, "principalBound": true, "principalCollapsed": false,
        "turns": [
            {"turnId": item, "kind": "item", "task": task,
             "state": "received", "content": "status poll", "ordinal": 0},
            {"turnId": "rr_status", "kind": "reply", "inReplyTo": item,
             "replyKind": "status", "authorship": "agent-composed",
             "content": RAW_STATUS_CANARY, "status": status, "ordinal": 1}
        ]
    })
}

// A fresh runtime is the existing board-fixture convention for independent
// reads: no sleeps and no bypass of the production refresh-floor guard.
async fn command_board(base: &Sender, peers: Vec<A2aPeerSpec>) -> String {
    let mut reader = Sender::with_journal_and_clock(
        peers,
        base.journal.room.clone(),
        base.journal.clone(),
        base.clock.clone(),
    )
    .await;
    command_board_in_process(&mut reader).await
}

async fn command_board_in_process(reader: &mut Sender) -> String {
    reader
        .command(&mut TuiState::new(80, 24), "/team board --last")
        .await;
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            match reader.domain_rx.recv().await {
                Some(AppEvent::TeamBoardReady { message, .. }) => return message,
                Some(_) => {}
                None => panic!("board event stream closed"),
            }
        }
    })
    .await
    .expect("production board command publishes its rendered report")
}

fn compact(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn malformed_statuses() -> Vec<(&'static str, Value)> {
    let mut cases = vec![
        ("null statement", Value::Null),
        ("array statement", json!([])),
        ("string statement", json!("minimal")),
        ("number statement", json!(1)),
        ("boolean statement", json!(true)),
        ("missing stories", json!({"level":"minimal"})),
        ("null stories", json!({"stories":null})),
        ("object stories", json!({"stories":{}})),
        ("string stories", json!({"stories":"S7-03"})),
        (
            "non-object entry",
            json!({"level":"minimal","stories":["S7-03"]}),
        ),
        ("null entry", json!({"level":"minimal","stories":[null]})),
        ("missing key", json!({"level":"minimal","stories":[{}]})),
        (
            "non-string key",
            json!({"level":"minimal","stories":[{"key":7}]}),
        ),
        (
            "invalid key",
            json!({"level":"minimal","stories":[{"key":"bad key"}]}),
        ),
        (
            "duplicate key",
            json!({"level":"minimal","stories":[{"key":"S7-03"},{"key":"S7-03"}]}),
        ),
        (
            "missing non-empty level",
            json!({"stories":[{"key":"S7-03"}]}),
        ),
        (
            "null non-empty level",
            json!({"level":null,"stories":[{"key":"S7-03"}]}),
        ),
        (
            "non-string level",
            json!({"level":7,"stories":[{"key":"S7-03"}]}),
        ),
        (
            "unknown level",
            json!({"level":"full","stories":[{"key":"S7-03"}]}),
        ),
        (
            "none with stories",
            json!({"level":"none","stories":[{"key":"S7-03"}]}),
        ),
        (
            "minimal empty level",
            json!({"level":"minimal","stories":[]}),
        ),
        ("status empty level", json!({"level":"status","stories":[]})),
        ("none empty level", json!({"level":"none","stories":[]})),
        ("null empty level", json!({"level":null,"stories":[]})),
        ("unknown empty level", json!({"level":"full","stories":[]})),
        (
            "missing status state",
            json!({"level":"status","stories":[{"key":"S7-03"}]}),
        ),
        (
            "null status state",
            json!({"level":"status","stories":[{"key":"S7-03","state":null}]}),
        ),
        (
            "non-string status state",
            json!({"level":"status","stories":[{"key":"S7-03","state":true}]}),
        ),
        (
            "unknown status state",
            json!({"level":"status","stories":[{"key":"S7-03","state":"working"}]}),
        ),
        (
            "received below status",
            json!({"level":"minimal","stories":[{"key":"S7-03","state":"received"}]}),
        ),
        (
            "null state below status",
            json!({"level":"minimal","stories":[{"key":"S7-03","state":null}]}),
        ),
    ];
    // Every entry is otherwise valid and unique: only the 64-entry bound fails.
    let entries = (1..=65)
        .map(|index| json!({"key":format!("S7-{index:02}")}))
        .collect::<Vec<_>>();
    cases.push(("65 entries", json!({"level":"minimal","stories":entries})));
    cases
}

async fn assert_shared_control(sender: &Sender, peer: &ControlledPeer) {
    let board = command_board(sender, vec![peer.spec.clone()]).await;
    let text = compact(&board);
    assert!(board.contains("S7-03"), "{board}");
    assert!(
        text.contains("Source:manuallyshared(member'spolicy:minimal)[agent-composed]"),
        "{board}"
    );
    assert!(
        !board.contains("status not readable by this build"),
        "{board}"
    );
    assert!(!board.contains(RAW_STATUS_CANARY), "{board}");
}

#[tokio::test]
async fn s_k04_every_malformed_status_is_unknown_and_never_leaks_raw_content() {
    let peer = controlled_pinned_peer(
        "member",
        false,
        Some("received"),
        ed25519_dalek::SigningKey::from_bytes(&[84; 32]),
    )
    .await;
    let mut sender = Sender::start(vec![peer.spec.clone()]).await;
    let mut state = TuiState::new(80, 24);
    sender
        .command(
            &mut state,
            "/team send --type status_request member status poll",
        )
        .await;
    sender.settle(&mut state, 1).await;
    assert_eq!(folded_actions(&sender.journal).await.len(), 1);
    let task = peer.dispatched_task.lock().clone();
    assert!(!task.is_empty(), "real outgoing dispatch reached HTTP");
    let valid = json!({"level":"minimal","stories":[{"key":"S7-03"}]});
    let served = Arc::new(parking_lot::Mutex::new(status_thread(
        "ri_controlled",
        &task,
        valid.clone(),
    )));
    mount_result(&peer.server, "x-rustain-items/thread", served.clone()).await;
    assert_shared_control(&sender, &peer).await;

    for (case, malformed) in malformed_statuses() {
        *served.lock() = status_thread("ri_controlled", &task, malformed);
        let before = rpc_requests(&peer.server, "x-rustain-items/thread")
            .await
            .len();
        let board = command_board(&sender, vec![peer.spec.clone()]).await;
        assert_eq!(
            rpc_requests(&peer.server, "x-rustain-items/thread")
                .await
                .len(),
            before + 1,
            "{case}: one production thread read per report"
        );
        assert!(
            board.contains("status not readable by this build"),
            "{case}: {board}"
        );
        assert!(board.contains("Source: —"), "{case}: {board}");
        assert!(!board.contains(RAW_STATUS_CANARY), "{case}: {board}");
        assert!(
            !board.contains("S7-"),
            "{case}: malformed stories must not render: {board}"
        );
        let ThreadRead::Read { thread, .. } =
            read_thread(sender.runtime(), "member", "ri_controlled").await
        else {
            panic!("{case}: malformed status must remain a readable thread, not a refusal");
        };
        let reply = thread
            .turns
            .iter()
            .find(|turn| turn.reply)
            .expect("served reply");
        assert_eq!(reply.kind, ReplyKind::Unknown, "{case}");
        assert!(reply.status.is_none(), "{case}");
    }

    // A valid read after every malformed response still reports the key/source.
    *served.lock() = status_thread("ri_controlled", &task, valid);
    assert_shared_control(&sender, &peer).await;
    // The accepted side of the size boundary must not be rejected wholesale.
    let entries = (1..=64)
        .map(|index| json!({"key":format!("S7-{index:02}")}))
        .collect::<Vec<_>>();
    *served.lock() = status_thread(
        "ri_controlled",
        &task,
        json!({"level":"minimal","stories":entries}),
    );
    let board = command_board(&sender, vec![peer.spec.clone()]).await;
    assert!(compact(&board).contains("S7-64"), "{board}");
    assert!(
        !board.contains("status not readable by this build"),
        "{board}"
    );
}

#[tokio::test]
async fn s_k04_report_reads_newest_correlated_item_through_renamed_pin_once() {
    let key = ed25519_dalek::SigningKey::from_bytes(&[85; 32]);
    let original = controlled_pinned_peer("send-time", false, Some("received"), key.clone()).await;
    let mut sender = Sender::start(vec![original.spec.clone()]).await;
    let mut state = TuiState::new(80, 24);
    sender
        .command(
            &mut state,
            "/team send --type status_request send-time status poll",
        )
        .await;
    sender.settle(&mut state, 1).await;
    let task = original.dispatched_task.lock().clone();
    let journal = sender.journal.clone();
    drop(sender);

    let renamed = controlled_pinned_peer("renamed", false, Some("received"), key).await;
    *renamed.dispatched_task.lock() = task.clone();
    mount_result(
        &renamed.server,
        "x-rustain-items/list",
        Arc::new(parking_lot::Mutex::new(json!({"items":[
            {"itemId":"ri_controlled","task":task,"state":"received","ordinal":1},
            {"itemId":"ri_newest","task":task,"state":"received","ordinal":2},
            {"itemId":"ri_unrelated","task":"another-action","state":"received","ordinal":99}
        ]}))),
    )
    .await;
    mount_result(
        &renamed.server,
        "x-rustain-items/thread",
        Arc::new(parking_lot::Mutex::new(status_thread(
            "ri_newest",
            &task,
            json!({"level":"status","stories":[{"key":"S7-05","state":"acknowledged"}]}),
        ))),
    )
    .await;
    let reopened = journal.reopen().await;
    let reader =
        Sender::with_journal(vec![renamed.spec.clone()], reopened.room.clone(), reopened).await;
    let board = command_board(&reader, vec![renamed.spec.clone()]).await;
    let reads = rpc_requests(&renamed.server, "x-rustain-items/thread").await;
    assert_eq!(
        reads.len(),
        1,
        "one report must use the single production thread reader"
    );
    assert_eq!(reads[0]["params"]["itemId"], "ri_newest");
    assert!(compact(&board).contains("S7-05acknowledged"), "{board}");
    assert!(
        compact(&board).contains("Source:manuallyshared(send-time'spolicy:status)[agent-composed]"),
        "{board}"
    );
    assert!(
        !board.contains("renamed"),
        "display preserves the send-time alias: {board}"
    );
    assert!(!board.contains(RAW_STATUS_CANARY), "{board}");
    assert!(
        rpc_requests(&original.server, "x-rustain-items/thread")
            .await
            .is_empty()
    );
}

async fn reply(client: &mut AttachedClient, item: &str, text: &str) {
    client
        .send_frame(ClientFrame::ReplyToRecipientItem {
            item_id: item.to_owned(),
            text: text.to_owned(),
            verdict: None,
        })
        .await;
    let result = client
        .wait_for(|frame| {
            matches!(frame, DaemonFrame::RecipientActionResult {
            action: RecipientAction::Reply, target: RecipientActionTarget::Item { item_id }, ..
        } if item_id == item)
        })
        .await;
    assert!(
        matches!(
            result,
            DaemonFrame::RecipientActionResult {
                outcome: RecipientActionOutcome::Applied { .. },
                ..
            }
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn s_k04_linked_followup_answer_is_ignored_and_newest_root_answer_is_shown() {
    let mut host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let socket = host.enable_operator().await;
    let mut operator = AttachedClient::attach(&socket, ConnectionTier::TrustedLocal, false).await;
    let mut sender = Sender::start(vec![host.peer("member")]).await;
    let mut state = TuiState::new(80, 24);
    sender
        .command(
            &mut state,
            "/team send --type status_request member root-status-poll",
        )
        .await;
    sender.settle(&mut state, 1).await;
    let root = read_room_events(&host.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address, content, ..
            } if content == "root-status-poll" => Some(address.item().as_str().to_owned()),
            _ => None,
        })
        .expect("real status poll ingress");
    reply(&mut operator, &root, "root-old-note").await;
    reply(&mut operator, &root, "root-current-note").await;
    sender
        .command(
            &mut state,
            &format!("/team followup member {root} linked-followup-body"),
        )
        .await;
    sender.settle(&mut state, 1).await;
    let followup = read_room_events(&host.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address,
                content,
                in_reply_to: Some(parent),
                ..
            } if content == "linked-followup-body" && parent.as_str() == root => {
                Some(address.item().as_str().to_owned())
            }
            _ => None,
        })
        .expect("production followup is linked to the root item");
    reply(&mut operator, &followup, "followup-answer-canary").await;
    assert_eq!(folded_actions(&sender.journal).await.len(), 1);
    let before = host.method_count("x-rustain-items/thread");
    let board = command_board_in_process(&mut sender).await;
    assert_eq!(host.method_count("x-rustain-items/thread"), before + 1);
    assert!(board.contains("\"root-current-note\""), "{board}");
    assert!(board.contains("Source: manually shared"), "{board}");
    assert!(!board.contains("root-old-note"), "{board}");
    assert!(!board.contains("followup-answer-canary"), "{board}");
    assert!(
        !board.contains("[agent-composed]"),
        "human reply has served human authorship: {board}"
    );
    let ThreadRead::Read { thread, .. } = read_thread(sender.runtime(), "member", &root).await
    else {
        panic!("the real root thread must be readable");
    };
    assert!(
        thread.turns.iter().any(|turn| {
            turn.reply
                && turn.in_reply_to.as_deref() == Some(followup.as_str())
                && turn.content.as_deref() == Some("followup-answer-canary")
        }),
        "the ignored followup Answer really was present in the served root thread"
    );
    operator.detach().await;
    host.stop().await;
}
