//! Story 19.25 r_k04: distrust raw served retrospective payloads, count only
//! the Retro turn answering the report's own item, and name omitted turns.
//! Every fixture is raw thread JSON served over controlled HTTP; every read
//! enters through `send::read_thread` and the production `/team board --last`.
//! Legacy literal-journal byte identity is covered by the domain fold units;
//! here a literal legacy (kind-less) reply turn travels the real HTTP parser.
use super::harness::{Sender, folded_actions};
use super::k02_board_rules::{ControlledPeer, controlled_pinned_peer};
use rustain::adapters::a2a::send::{ThreadRead, read_thread};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{A2aPeerSpec, RETRO_REPLY_TEXT, ReplyKind};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ITEM: &str = "ri_controlled";
const THREAD: &str = "x-rustain-items/thread";
const UNREADABLE: &str = "Some replies are not readable by this build.";
const OMITTED: &str = "Some replies may be in thread turns a peer did not serve.";
const HEADER: &str = "Retrospective report · retro_request";
const NOTHING: &str = "Nothing has been shared so far.";

type Served = Arc<parking_lot::Mutex<Value>>;

async fn mount_result(server: &MockServer, rpc_method: &str, result: Served) {
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

async fn thread_reads(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("recorded HTTP requests")
        .into_iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|request| request["method"] == THREAD)
        .count()
}

fn item_turn(id: &str, task: &str, ordinal: u64) -> Value {
    json!({"turnId": id, "kind": "item", "task": task,
           "state": "received", "content": "retro poll", "ordinal": ordinal})
}

fn retro_turn(id: &str, answers: &str, retro: Value, ordinal: u64) -> Value {
    json!({"turnId": id, "kind": "reply", "inReplyTo": answers,
           "replyKind": "retro", "authorship": "agent-composed",
           "content": RETRO_REPLY_TEXT, "retro": retro, "ordinal": ordinal})
}

/// A literal legacy reply: no `replyKind`, no `retro`, as old hosts serve it.
fn legacy_turn(answers: &str, ordinal: u64) -> Value {
    json!({"turnId": "rr_legacy", "kind": "reply", "inReplyTo": answers,
           "authorship": "agent-composed", "content": "legacy words", "ordinal": ordinal})
}

fn thread(turns: Vec<Value>, omitted_before: u64) -> Value {
    json!({"root": ITEM, "principalBound": true, "principalCollapsed": false,
           "omittedBefore": omitted_before, "turns": turns})
}

fn valid_retro() -> Value {
    json!({"received":{"consultation":3},"replyTimes":[4,25,90],"ackTimes":[60]})
}

fn retro_thread(task: &str, retro: Value, omitted_before: u64) -> Value {
    thread(
        vec![
            item_turn(ITEM, task, 0),
            retro_turn("rr_retro", ITEM, retro, 1),
        ],
        omitted_before,
    )
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

/// One counted production read per report, through a fresh reader runtime.
async fn counted_board(sender: &Sender, peer: &ControlledPeer, case: &str) -> String {
    let before = thread_reads(&peer.server).await;
    let board = command_board(sender, vec![peer.spec.clone()]).await;
    assert_eq!(
        thread_reads(&peer.server).await,
        before + 1,
        "{case}: one production thread read per report"
    );
    board
}

async fn started(seed: u8, kind: &str) -> (ControlledPeer, Sender, String) {
    let peer = controlled_pinned_peer(
        "member",
        false,
        Some("received"),
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32]),
    )
    .await;
    let mut sender = Sender::start(vec![peer.spec.clone()]).await;
    let mut state = TuiState::new(80, 24);
    sender
        .command(
            &mut state,
            &format!("/team send --type {kind} member retro poll"),
        )
        .await;
    sender.settle(&mut state, 1).await;
    assert_eq!(folded_actions(&sender.journal).await.len(), 1);
    let task = peer.dispatched_task.lock().clone();
    assert!(!task.is_empty(), "real outgoing dispatch reached HTTP");
    (peer, sender, task)
}

/// The valid raw contribution is counted; no note is raised.
fn assert_counted(board: &str, case: &str) {
    assert!(board.contains(HEADER), "{case}: {board}");
    assert!(
        board.contains("Received items (14 days): consultation 3"),
        "{case}: {board}"
    );
    assert!(
        board.contains("Time to first reply: median 25m · 3 replies"),
        "{case}: {board}"
    );
    assert!(
        board.contains("Time to acknowledge: median 1h 0m · 1 "),
        "{case}: {board}"
    );
    assert!(!board.contains(NOTHING), "{case}: {board}");
    assert!(!board.contains(UNREADABLE), "{case}: {board}");
}

async fn assert_reader_kind(sender: &Sender, case: &str, expected: ReplyKind, counted: bool) {
    let ThreadRead::Read { thread, .. } = read_thread(sender.runtime(), "member", ITEM).await
    else {
        panic!("{case}: a retro payload must never refuse a readable thread");
    };
    let reply = thread
        .turns
        .iter()
        .find(|turn| turn.reply && turn.turn_id == "rr_retro")
        .expect("served retro reply");
    assert_eq!(reply.kind, expected, "{case}");
    assert_eq!(reply.retro.is_some(), counted, "{case}");
    assert_eq!(reply.content.as_deref(), Some(RETRO_REPLY_TEXT), "{case}");
}

fn malformed_retros() -> Vec<(&'static str, Value)> {
    let over: Vec<u32> = (0..513).collect();
    vec![
        ("null", Value::Null),
        ("array", json!([])),
        ("string", json!("retro")),
        ("number", json!(1)),
        ("boolean", json!(true)),
        ("unknown key alone", json!({"senders":[]})),
        (
            "unknown key beside valid",
            json!({"received":{"consultation":3},"senders":["a"]}),
        ),
        ("empty received", json!({"received":{}})),
        ("empty replyTimes", json!({"replyTimes":[]})),
        ("empty ackTimes", json!({"ackTimes":[]})),
        ("non-object received", json!({"received":[]})),
        ("unknown token", json!({"received":{"gossip":1}})),
        ("zero count", json!({"received":{"consultation":0}})),
        (
            "count above bound",
            json!({"received":{"consultation":1_000_001}}),
        ),
        ("negative count", json!({"received":{"consultation":-1}})),
        (
            "non-numeric count",
            json!({"received":{"consultation":"3"}}),
        ),
        ("513 samples", json!({"replyTimes":over})),
        ("negative sample", json!({"replyTimes":[-1]})),
        ("sample 20161", json!({"replyTimes":[20_161]})),
        ("unsorted replyTimes", json!({"replyTimes":[25,4]})),
        ("unsorted ackTimes", json!({"ackTimes":[60,1]})),
        ("ack sample 20161", json!({"ackTimes":[20_161]})),
        ("non-numeric sample", json!({"replyTimes":["4"]})),
        ("fractional sample", json!({"replyTimes":[4.5]})),
        ("non-array list", json!({"replyTimes":4})),
    ]
}

#[tokio::test]
async fn r_k04_every_malformed_retro_is_unknown_excluded_and_noted_once() {
    let (peer, sender, task) = started(94, "retro_request").await;
    let served = Arc::new(parking_lot::Mutex::new(thread(
        vec![
            item_turn(ITEM, &task, 0),
            legacy_turn(ITEM, 1),
            retro_turn("rr_retro", ITEM, valid_retro(), 2),
        ],
        0,
    )));
    mount_result(&peer.server, THREAD, served.clone()).await;
    // Positive control: raw valid payload beside a literal legacy reply.
    let board = counted_board(&sender, &peer, "valid control").await;
    assert_counted(&board, "valid control");
    assert!(!board.contains(OMITTED), "{board}");
    assert!(!board.contains("legacy words"), "{board}");
    assert_reader_kind(&sender, "valid control", ReplyKind::Retro, true).await;

    for (case, malformed) in malformed_retros() {
        *served.lock() = retro_thread(&task, malformed, 0);
        let board = counted_board(&sender, &peer, case).await;
        assert!(board.contains(HEADER), "{case}: {board}");
        assert_eq!(board.matches(UNREADABLE).count(), 1, "{case}: {board}");
        assert!(board.contains(NOTHING), "{case}: excluded: {board}");
        for leak in [
            "consultation",
            "Time to first reply",
            "Time to acknowledge",
            "member",
        ] {
            assert!(!board.contains(leak), "{case}: {leak}: {board}");
        }
        assert_reader_kind(&sender, case, ReplyKind::Unknown, false).await;
    }

    // A missing payload on a retro turn is equally unreadable.
    let mut missing = retro_thread(&task, Value::Null, 0);
    missing["turns"][1].as_object_mut().unwrap().remove("retro");
    *served.lock() = missing;
    let board = counted_board(&sender, &peer, "missing payload").await;
    assert_eq!(board.matches(UNREADABLE).count(), 1, "{board}");
    assert!(board.contains(NOTHING), "{board}");

    // Accepted side of every bound is counted, not rejected wholesale.
    let mut edge: Vec<u32> = (0..511).collect();
    edge.push(20_160);
    *served.lock() = retro_thread(
        &task,
        json!({"received":{"consultation":1_000_000},"replyTimes":edge,"ackTimes":[20_160]}),
        0,
    );
    let board = counted_board(&sender, &peer, "bounds").await;
    assert!(board.contains("consultation 1000000"), "{board}");
    assert!(
        board.contains("Time to first reply: median 4h 15m · 512 replies"),
        "{board}"
    );
    assert!(
        board.contains("Time to acknowledge: median 14d 0h · 1 "),
        "{board}"
    );
    assert!(!board.contains(UNREADABLE), "{board}");
    assert_reader_kind(&sender, "bounds", ReplyKind::Retro, true).await;

    // Recovery after the whole matrix: the valid control still counts.
    *served.lock() = retro_thread(&task, valid_retro(), 0);
    assert_counted(&counted_board(&sender, &peer, "recovery").await, "recovery");
}

#[tokio::test]
async fn r_k04_followup_and_sibling_retro_turns_are_never_counted_for_the_item() {
    let (peer, sender, task) = started(95, "retro_request").await;
    let foreign = json!({"received":{"story_assignment":7},"replyTimes":[1000],"ackTimes":[2000]});
    let served = Arc::new(parking_lot::Mutex::new(thread(
        vec![
            item_turn(ITEM, &task, 0),
            item_turn("ri_followup", &task, 1),
            retro_turn("rr_followup", "ri_followup", foreign.clone(), 2),
            retro_turn("rr_sibling", "ri_sibling", foreign.clone(), 3),
        ],
        0,
    )));
    mount_result(&peer.server, THREAD, served.clone()).await;
    let board = counted_board(&sender, &peer, "foreign only").await;
    assert!(board.contains(HEADER), "{board}");
    assert!(board.contains(NOTHING), "{board}");
    assert!(!board.contains("story_assignment"), "{board}");
    assert!(!board.contains(UNREADABLE), "{board}");
    // The reader sees the foreign turns as typed Retro: exclusion is the
    // board's item scoping, not a parse failure.
    let ThreadRead::Read { thread: read, .. } = read_thread(sender.runtime(), "member", ITEM).await
    else {
        panic!("thread readable");
    };
    assert_eq!(
        read.turns
            .iter()
            .filter(|turn| turn.kind == ReplyKind::Retro && turn.retro.is_some())
            .count(),
        2
    );

    // Counted parent positive control alongside the same foreign turns.
    let mut with_parent = served.lock().clone();
    with_parent["turns"]
        .as_array_mut()
        .unwrap()
        .push(retro_turn("rr_retro", ITEM, valid_retro(), 4));
    *served.lock() = with_parent;
    let board = counted_board(&sender, &peer, "parent counted").await;
    assert_counted(&board, "parent counted");
    assert!(!board.contains("story_assignment"), "{board}");
    assert!(!board.contains("16h"), "foreign samples merged: {board}");
}

#[tokio::test]
async fn r_k04_omitted_thread_turns_yield_generic_retro_note_even_with_valid_turn() {
    let (peer, sender, task) = started(96, "retro_request").await;
    let served = Arc::new(parking_lot::Mutex::new(retro_thread(
        &task,
        valid_retro(),
        0,
    )));
    mount_result(&peer.server, THREAD, served.clone()).await;
    let board = counted_board(&sender, &peer, "nothing omitted").await;
    assert_counted(&board, "nothing omitted");
    assert!(!board.contains(OMITTED), "{board}");

    *served.lock() = retro_thread(&task, valid_retro(), 3);
    let board = counted_board(&sender, &peer, "omitted with valid").await;
    assert_counted(&board, "omitted with valid");
    assert_eq!(board.matches(OMITTED).count(), 1, "{board}");
    assert!(!board.contains("3 turns"), "no count: {board}");
    assert!(!board.contains("member"), "{board}");

    *served.lock() = thread(vec![item_turn(ITEM, &task, 0)], 2);
    let board = counted_board(&sender, &peer, "omitted only").await;
    assert_eq!(board.matches(OMITTED).count(), 1, "{board}");
    assert!(board.contains(NOTHING), "{board}");
}

fn status_turn(status: Value) -> Value {
    json!({"turnId": "rr_status", "kind": "reply", "inReplyTo": ITEM,
           "replyKind": "status", "authorship": "agent-composed",
           "content": "status shared", "status": status, "ordinal": 1})
}

#[tokio::test]
async fn r_k04_status_row_names_omitted_turns_unless_a_status_turn_was_served() {
    const OMITTED_ROW: &str = "status not among the turns this peer served";
    let (peer, sender, task) = started(97, "status_request").await;
    let served = Arc::new(parking_lot::Mutex::new(thread(
        vec![item_turn(ITEM, &task, 0)],
        0,
    )));
    mount_result(&peer.server, THREAD, served.clone()).await;
    // Control: nothing omitted, no Status turn → the 19-24 none row.
    let board = counted_board(&sender, &peer, "none control").await;
    assert!(!board.contains(OMITTED_ROW), "{board}");
    assert!(board.contains("member"), "{board}");

    *served.lock() = thread(vec![item_turn(ITEM, &task, 0), legacy_turn(ITEM, 1)], 2);
    let board = counted_board(&sender, &peer, "omitted").await;
    assert!(board.contains(OMITTED_ROW), "{board}");
    assert!(board.contains("Source: —"), "{board}");
    assert!(!board.contains("S7-03"), "{board}");

    // A served valid Status turn overrides the omitted row.
    *served.lock() = thread(
        vec![
            item_turn(ITEM, &task, 0),
            status_turn(json!({"level":"minimal","stories":[{"key":"S7-03"}]})),
        ],
        2,
    );
    let board = counted_board(&sender, &peer, "status served").await;
    assert!(board.contains("S7-03"), "{board}");
    assert!(!board.contains(OMITTED_ROW), "{board}");
    assert!(!board.contains("Source: —"), "{board}");
}
