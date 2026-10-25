//! Story 19.25 r_k02: not taking part is indistinguishable from every other
//! way of contributing nothing, and no retrospective surface names or counts
//! anyone (D3, D5, D6, D7; M04, M05, M06, M07, M08).
//!
//! Every fact here enters through a front door: attached `TeamSend`, the
//! recipient's real wait card and `ShareRecipientRetro` act, the recipient's
//! real served `x-rustain-items/thread`, `/team board --last`, and the
//! standalone `/team send` rows painted by the production settlement path.
use super::attached_harness::{AttachedClient, AttachedDaemon, RecipientHost};
use super::harness::{Journal, Sender, read_room_events};
use super::r_harness::{act, board, clocked_host, replies, reply, retro_policy, send};
use super::s_harness::{host_item, wait_host_control};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    ConnectionTier, RecipientActionOutcome, TeamSendOutcome,
};
use rustain::adapters::tui::handlers::team_command::{TEAM_SEND_RETRO_NOTE, TeamBoardSelector};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    ItemAddress, JournalRecord, RETRO_HONESTY_LINE, RETRO_REPLY_TEXT, RetroField, RoomEvent,
    SemanticMessageType, SendSettlement,
};
use rustain::infrastructure::runtime::transparency_bridge::team_board_message;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Every host in every run starts at the same fixed wall time.
const T0: i64 = 1_800_000_000_000;
const THREAD: &str = "x-rustain-items/thread";
const B: &str = "bravo-member";
const C: &str = "charlie-member";
const D: &str = "delta-member";
/// B answers its seed item this many minutes after it arrived, in every run.
const B_REPLY_MINUTES: u64 = 25;
/// C's own facts, present whenever C has any: they must never surface.
const C_REPLY_MINUTES: u64 = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Decline,
    NothingThenShare,
    Unanswered,
    LeaveOutEveryField,
    Alone,
}

struct Run {
    report: String,
    b_turn: Value,
    /// C's real served Retro reply turn, for the three cases that answer.
    c_turn: Option<Value>,
}

/// One real seed item answered after `minutes` on the recipient's clock.
async fn seed(
    writer: &mut AttachedClient,
    host: &RecipientHost,
    operator: &mut AttachedClient,
    clock: &MockClock,
    key: &str,
    alias: &str,
    minutes: u64,
) {
    let outcomes = send(
        writer,
        key,
        &[alias],
        SemanticMessageType::Consultation,
        "private seed question",
    )
    .await;
    assert!(
        matches!(outcomes.as_slice(), [TeamSendOutcome::Delivered]),
        "{outcomes:?}"
    );
    let item = host_item(host, SemanticMessageType::Consultation).await;
    wait_host_control(operator, &item).await;
    clock.advance(Duration::from_secs(minutes * 60));
    reply(operator, &item, "private seed answer").await;
}

/// The recipient's real served reply turns for `item`, read with the item
/// capability its real `retro_request` POST carried.
async fn served_replies(host: &RecipientHost, item: &ItemAddress) -> Vec<Value> {
    let capability = host
        .message_send_posts()
        .into_iter()
        .find_map(|post| {
            let metadata = &post["params"]["message"]["metadata"];
            (metadata["x-rustain-message-type"] == "retro_request")
                .then(|| metadata["x-rustain-item-capability"].clone())
        })
        .expect("the real retro_request POST reached this host");
    assert!(
        capability.is_string(),
        "item capability carrier: {capability}"
    );
    let wire: Value = reqwest::Client::new()
        .post(&host.endpoint)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": THREAD,
            "params": {"itemId": item.item().as_str(), "capability": capability},
        }))
        .send()
        .await
        .expect("served thread request")
        .json()
        .await
        .expect("served thread JSON");
    assert!(wire["error"].is_null(), "{wire}");
    wire["result"]["turns"]
        .as_array()
        .expect("real served thread turns")
        .iter()
        .filter(|turn| turn["kind"] == "reply")
        .cloned()
        .collect()
}

fn keys(turn: &Value) -> Vec<String> {
    let mut keys = turn
        .as_object()
        .expect("served turn object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

/// One fresh, fully isolated fixture: A (clocked journal + egress), B and C
/// on their own clocks. B is seeded and shares identically in every run.
async fn no_participation_run(case: Case) -> Run {
    let a_clock = Arc::new(MockClock::at_wall_ms(T0));
    let b_clock = Arc::new(MockClock::at_wall_ms(T0));
    let c_clock = Arc::new(MockClock::at_wall_ms(T0));
    let (b, mut b_operator) = clocked_host(b_clock.clone()).await;
    let mut c = match case {
        Case::Alone => None,
        _ => Some(clocked_host(c_clock.clone()).await),
    };
    let mut peers = vec![b.peer(B)];
    if let Some((host, _)) = &c {
        peers.push(host.peer(C));
    }
    let journal = Journal::open_with_clock(a_clock.clone()).await;
    let room = journal.room.clone();
    let daemon = AttachedDaemon::compose_with_clock(journal, peers, room, a_clock).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    seed(
        &mut writer,
        &b,
        &mut b_operator,
        &b_clock,
        "seed-b",
        B,
        B_REPLY_MINUTES,
    )
    .await;
    // Everything seeded is strictly before the request's arrival.
    b_clock.advance(Duration::from_millis(1));
    if let Some((host, operator)) = c.as_mut() {
        if case != Case::NothingThenShare {
            seed(
                &mut writer,
                host,
                operator,
                &c_clock,
                "seed-c",
                C,
                C_REPLY_MINUTES,
            )
            .await;
        }
        c_clock.advance(Duration::from_millis(1));
    }

    let recipients: &[&str] = if c.is_some() { &[B, C] } else { &[B] };
    let outcomes = send(
        &mut writer,
        "retro",
        recipients,
        SemanticMessageType::RetroRequest,
        "Sprint retro: private notes and transcripts please",
    )
    .await;
    assert_eq!(outcomes.len(), recipients.len(), "{case:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, TeamSendOutcome::Delivered)),
        "{case:?}: {outcomes:?}"
    );

    // B: the real snapshot carries B's facts; B shares all of it.
    let b_item = host_item(&b, SemanticMessageType::RetroRequest).await;
    let b_control = wait_host_control(&mut b_operator, &b_item).await;
    let b_preview = serde_json::to_value(&b_control.retro).unwrap();
    assert_eq!(b_preview["nothingInWindow"], false, "{case:?}: {b_preview}");
    assert_eq!(
        b_preview["contribution"]["received"],
        json!({"consultation": 1}),
        "{case:?}: {b_preview}"
    );
    assert_eq!(
        b_preview["contribution"]["replyTimes"],
        json!([B_REPLY_MINUTES]),
        "{case:?}: {b_preview}"
    );
    assert!(
        matches!(
            act(&mut b_operator, &b_item, Vec::new(), false).await,
            RecipientActionOutcome::Applied { .. }
        ),
        "{case:?}"
    );
    assert_eq!(replies(&b, &b_item).await.len(), 1, "{case:?}");

    // C: the real snapshot holds C's own facts except in the nothing case,
    // so a decline or an all-left-out share has something to withhold.
    let mut c_item = None;
    if let Some((host, operator)) = c.as_mut() {
        let item = host_item(host, SemanticMessageType::RetroRequest).await;
        let control = wait_host_control(operator, &item).await;
        let preview = serde_json::to_value(&control.retro).unwrap();
        if case == Case::NothingThenShare {
            assert_eq!(
                preview,
                json!({"contribution": {}, "nothingInWindow": true}),
                "{case:?}"
            );
        } else {
            assert_eq!(preview["nothingInWindow"], false, "{case:?}: {preview}");
            assert_eq!(
                preview["contribution"]["received"],
                json!({"consultation": 1}),
                "{case:?}: positive facts C withholds: {preview}"
            );
            assert_eq!(
                preview["contribution"]["replyTimes"],
                json!([C_REPLY_MINUTES]),
                "{case:?}: positive facts C withholds: {preview}"
            );
        }
        let outcome = match case {
            Case::Decline => Some(act(operator, &item, Vec::new(), true).await),
            Case::NothingThenShare => Some(act(operator, &item, Vec::new(), false).await),
            Case::LeaveOutEveryField => {
                Some(act(operator, &item, RetroField::ALL.to_vec(), false).await)
            }
            Case::Unanswered => None,
            Case::Alone => unreachable!("C exists only when addressed"),
        };
        if let Some(outcome) = outcome {
            assert!(
                matches!(outcome, RecipientActionOutcome::Applied { .. }),
                "{case:?}: {outcome:?}"
            );
        }
        assert_eq!(
            replies(host, &item).await.len(),
            usize::from(case != Case::Unanswered),
            "{case:?}: at most one Retro turn, none unanswered"
        );
        c_item = Some(item);
    }

    assert_eq!(b.method_count(THREAD), 0, "{case:?}");
    let report = board(&mut writer, "retro-report").await;
    assert_eq!(b.method_count(THREAD), 1, "{case:?}: one read per row");
    if let Some((host, _)) = &c {
        assert_eq!(host.method_count(THREAD), 1, "{case:?}: one read per row");
    }

    // Positive control: B's real served turn is exactly its snapshot.
    let b_served = served_replies(&b, &b_item).await;
    assert_eq!(b_served.len(), 1, "{case:?}: {b_served:?}");
    let b_turn = b_served[0].clone();
    assert_eq!(b_turn["replyKind"], "retro", "{case:?}: {b_turn}");
    assert_eq!(
        b_turn["retro"], b_preview["contribution"],
        "{case:?}: {b_turn}"
    );
    assert_eq!(b_turn["content"], RETRO_REPLY_TEXT, "{case:?}: {b_turn}");

    let c_turn = match (&c, &c_item) {
        (Some((host, _)), Some(item)) => {
            let turns = served_replies(host, item).await;
            if case == Case::Unanswered {
                assert!(turns.is_empty(), "{case:?}: {turns:?}");
                None
            } else {
                assert_eq!(turns.len(), 1, "{case:?}: {turns:?}");
                Some(turns[0].clone())
            }
        }
        _ => None,
    };

    assert!(
        b.requests.lock().is_empty(),
        "{case:?}: B made no provider request"
    );
    if let Some((host, _)) = &c {
        assert!(
            host.requests.lock().is_empty(),
            "{case:?}: C made no provider request"
        );
    }
    assert_eq!(daemon.provider_request_count(), 0, "{case:?}");

    writer.detach().await;
    b_operator.detach().await;
    if let Some((host, operator)) = c {
        operator.detach().await;
        host.stop().await;
    }
    daemon.stop().await;
    b.stop().await;
    Run {
        report,
        b_turn,
        c_turn,
    }
}

/// M04/M05/M06/M07: {C declines}, {C holds nothing and shares}, {C never
/// answers}, {C leaves out every field} and {B alone} render one report, byte
/// for byte, that names and counts nobody; C's three answering turns are the
/// same canonical `{}` on the served wire although C held real facts.
#[tokio::test]
async fn r_k02_no_participation_reports_are_byte_identical_and_name_nobody() {
    let mut runs = Vec::new();
    for case in [
        Case::Decline,
        Case::NothingThenShare,
        Case::Unanswered,
        Case::LeaveOutEveryField,
        Case::Alone,
    ] {
        runs.push((case, no_participation_run(case).await));
    }
    let reference = runs[0].1.report.clone();
    for (case, run) in &runs {
        let report = &run.report;
        // Positive control: B's non-empty share is the report's content.
        assert!(
            report.starts_with("Retrospective report · retro_request\nattempted just now\n"),
            "{case:?}: {report}"
        );
        assert!(
            report.contains("\n  Received items (14 days): consultation 1\n"),
            "{case:?}: {report}"
        );
        assert!(
            report.contains(&format!(
                "\n  Time to first reply: median {B_REPLY_MINUTES}m · "
            )),
            "{case:?}: {report}"
        );
        assert!(
            report.ends_with(&format!("{RETRO_HONESTY_LINE}\n")),
            "{case:?}: {report}"
        );
        for forbidden in [
            B,
            C,
            "member",
            "addressed",
            "Distribution board",
            "declined",
            "awaiting",
            "Nothing has been shared",
            "Some ",
            "consultation 2",
            "40m",
            "private",
            "✗",
            "●",
            "✓",
        ] {
            assert!(
                !report.contains(forbidden),
                "{case:?}: `{forbidden}` in {report}"
            );
        }
        assert_eq!(report, &reference, "{case:?} report differs from Decline");
        assert_eq!(run.b_turn["retro"], runs[0].1.b_turn["retro"], "{case:?}");
    }

    let answered = runs
        .iter()
        .filter_map(|(case, run)| run.c_turn.as_ref().map(|turn| (*case, turn, &run.b_turn)))
        .collect::<Vec<_>>();
    assert_eq!(
        answered.iter().map(|(case, ..)| *case).collect::<Vec<_>>(),
        [
            Case::Decline,
            Case::NothingThenShare,
            Case::LeaveOutEveryField
        ]
    );
    let (_, first, _) = answered[0];
    for (case, turn, b_turn) in &answered {
        assert_eq!(turn["replyKind"], "retro", "{case:?}: {turn}");
        assert_eq!(
            turn["retro"],
            json!({}),
            "{case:?}: canonical nothing: {turn}"
        );
        assert_eq!(turn["content"], RETRO_REPLY_TEXT, "{case:?}: {turn}");
        assert_eq!(turn["authorship"], "agent-composed", "{case:?}: {turn}");
        let encoded = turn.to_string();
        for forbidden in [
            "replyTimes",
            "ackTimes",
            "nothingInWindow",
            "decline",
            "leaveOut",
            "leave_out",
            "sendAuthorization",
            "send_authorization",
            "private",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "{case:?}: `{forbidden}` in {encoded}"
            );
        }
        assert_eq!(
            (&turn["retro"], &turn["content"]),
            (&first["retro"], &first["content"]),
            "{case:?}"
        );
        // A decline has the very shape of a share: only the value differs.
        assert_eq!(keys(turn), keys(first), "{case:?}");
        assert_eq!(keys(turn), keys(b_turn), "{case:?}");
    }
}

/// M07/M08 on the standalone rail: D's host declines `retro_request` by
/// policy and its row reads `sent`, exactly like delivered B's, followed by
/// the D7 note; the retro board names and counts nobody. Control: the same
/// sends as `consultation` keep `✗ declined` and the `N addressed` board.
#[tokio::test]
async fn r_k02_policy_decline_reads_sent_and_consultation_control_keeps_declined() {
    let b =
        RecipientHost::start_with_effective_policy(A2aAdmissionPolicy::Allow, retro_policy()).await;
    let d = RecipientHost::start(A2aAdmissionPolicy::Deny).await;
    let mut sender = Sender::start(vec![b.peer(B), d.peer(D)]).await;
    let mut state = TuiState::new(100, 40);

    sender
        .command(
            &mut state,
            &format!("/team send --type retro_request {B},{D} Sprint retro"),
        )
        .await;
    assert_eq!(state.team_send_blocks.len(), 1, "real rows were painted");
    let retro_block = state.team_send_blocks.keys().next().unwrap().clone();
    sender.settle(&mut state, 2).await;
    let settled = read_room_events(&sender.journal)
        .await
        .into_iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::TeamSendRecipientSettled {
                index, settlement, ..
            } => Some((index, settlement)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(settled.len(), 2, "{settled:?}");
    assert!(
        settled.iter().any(|(index, settlement)| *index == 0
            && matches!(settlement, SendSettlement::Delivered { .. })),
        "{settled:?}"
    );
    assert!(
        settled
            .iter()
            .any(|(index, settlement)| *index == 1 && *settlement == SendSettlement::Declined),
        "D's host really declined by policy: {settled:?}"
    );
    assert!(
        !d.journal.entries().await.iter().any(|entry| matches!(
            entry.record,
            JournalRecord::Room(RoomEvent::RecipientItemReceived { .. })
        )),
        "a policy decline admits no item"
    );
    let retro_rows = state.feedback_blocks[&retro_block].message.clone();
    assert_eq!(
        retro_rows,
        format!("  ● {B}  sent\n  ● {D}  sent\n{TEAM_SEND_RETRO_NOTE}")
    );
    let retro_board = team_board_message(sender.runtime(), &TeamBoardSelector::Last).await;
    assert!(
        retro_board.starts_with("Retrospective report · retro_request\n"),
        "{retro_board}"
    );
    assert!(
        retro_board.contains("Nothing has been shared so far."),
        "{retro_board}"
    );
    for forbidden in [
        B,
        D,
        "addressed",
        "declined",
        "Distribution board",
        "✗",
        "●",
    ] {
        assert!(
            !retro_board.contains(forbidden),
            "`{forbidden}` in {retro_board}"
        );
    }
    assert_eq!(b.method_count(THREAD), 1, "B's delivered row was read");
    assert_eq!(
        d.method_count(THREAD),
        0,
        "a declined row has no item to read"
    );

    // Control: the same addressed hosts, as an ordinary consultation.
    sender
        .command(
            &mut state,
            &format!("/team send --type consultation {B},{D} Sprint question"),
        )
        .await;
    assert_eq!(state.team_send_blocks.len(), 1, "real rows were painted");
    let control_block = state.team_send_blocks.keys().next().unwrap().clone();
    assert_ne!(control_block, retro_block);
    sender.settle(&mut state, 2).await;
    let control_rows = state.feedback_blocks[&control_block].message.clone();
    assert_eq!(
        control_rows,
        format!("  ● {B}  delivered\n  ✗ {D}  declined")
    );
    assert!(!control_rows.contains(TEAM_SEND_RETRO_NOTE));
    // Independent board read, like the tolerance fixtures: preserve the real
    // refresh-floor guard without sleeping or weakening the control.
    let reader = Sender::with_journal_and_clock(
        vec![b.peer(B), d.peer(D)],
        sender.journal.room.clone(),
        sender.journal.clone(),
        sender.clock.clone(),
    )
    .await;
    let control_board = team_board_message(reader.runtime(), &TeamBoardSelector::Last).await;
    let declined_row = format!("✗ {D}");
    for expected in [
        "Distribution board",
        "2 addressed",
        B,
        declined_row.as_str(),
    ] {
        assert!(
            control_board.contains(expected),
            "`{expected}` missing: {control_board}"
        );
    }
    assert!(
        !control_board.contains("Retrospective report"),
        "{control_board}"
    );
    assert!(b.requests.lock().is_empty(), "B made no provider request");
    assert!(d.requests.lock().is_empty(), "D made no provider request");

    b.stop().await;
    d.stop().await;
}
