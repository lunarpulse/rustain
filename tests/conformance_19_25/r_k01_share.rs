//! Story 19.25 r_k01: consented retrospective contributions travel from two
//! clocked recipient hosts to the requester's aggregate report. Every fact is
//! produced by real sends, real recipient acts and real reads.
use super::attached_harness::{AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT, RecipientHost};
use super::r_harness::{act, board, clocked_host, replies, reply, send};
use super::s_harness::{host_item, wait_host_control};
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, RecipientAction, RecipientActionOutcome,
    RecipientActionTarget, TeamSendOutcome,
};
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    ItemAddress, JournalRecord, RetroField, RoomEvent, SemanticMessageType,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const B: &str = "bravo-member";
const C: &str = "charlie-member";
const RETRO_TEXT: &str = "Retrospective reply.";

#[derive(Clone, Copy)]
enum Act {
    Acknowledge,
    Reply,
}

async fn acknowledge(operator: &mut AttachedClient, item: &ItemAddress) {
    let item_id = item.item().as_str().to_owned();
    operator
        .send_frame(ClientFrame::AcknowledgeRecipientItem {
            item_id: item_id.clone(),
        })
        .await;
    let frame = operator
        .wait_for(|frame| {
            matches!(
                frame,
                DaemonFrame::RecipientActionResult {
                    action: RecipientAction::Acknowledge,
                    target: RecipientActionTarget::Item { item_id: actual },
                    ..
                } if *actual == item_id
            )
        })
        .await;
    assert!(
        matches!(
            frame,
            DaemonFrame::RecipientActionResult {
                outcome: RecipientActionOutcome::Applied { .. },
                ..
            }
        ),
        "{frame:?}"
    );
}

/// One real typed send from the requester, then the recipient operator's acts
/// at hand-known minute offsets from the item's arrival on the host clock.
#[allow(clippy::too_many_arguments)]
async fn exchange(
    writer: &mut AttachedClient,
    key: &str,
    alias: &str,
    kind: SemanticMessageType,
    host: &RecipientHost,
    operator: &mut AttachedClient,
    clock: &MockClock,
    acts: &[(u64, Act)],
) {
    let outcomes = send(writer, key, &[alias], kind, "private team note").await;
    assert!(
        matches!(outcomes.as_slice(), [TeamSendOutcome::Delivered]),
        "{key}: {outcomes:?}"
    );
    let item = host_item(host, kind).await;
    let card = wait_host_control(operator, &item).await;
    assert!(
        card.retro.is_none(),
        "{key}: only a retro_request card prepares a summary"
    );
    let mut elapsed = 0;
    for (offset, act) in acts {
        assert!(*offset >= elapsed, "offsets are listed in host-clock order");
        clock.advance(Duration::from_secs((offset - elapsed) * 60));
        elapsed = *offset;
        match act {
            Act::Acknowledge => acknowledge(operator, &item).await,
            Act::Reply => reply(operator, &item, "handled on this host").await,
        }
    }
}

async fn served_thread(host: &RecipientHost, item: &ItemAddress) -> Value {
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        reqwest::Client::new()
            .post(&host.endpoint)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1925,
                "method": "x-rustain-items/thread",
                "params": {"itemId": item.item().as_str()},
            }))
            .send()
            .await
            .expect("served thread request")
            .json()
            .await
            .expect("served thread JSON")
    })
    .await
    .expect("served thread deadline")
}

fn assert_no_local_facts(encoded: &str, surface: &str) {
    for forbidden in [
        "sendAuthorization",
        "send_authorization",
        "human-decided",
        "nothingInWindow",
        "nothing_in_window",
        "private team note",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "{surface} serves {forbidden}: {encoded}"
        );
    }
}

#[tokio::test]
async fn r_k01_consented_shares_aggregate_into_the_requesters_report() {
    let base = 1_800_000_000_000;
    let b_clock = Arc::new(MockClock::at_wall_ms(base));
    let c_clock = Arc::new(MockClock::at_wall_ms(base));
    let (b, mut b_operator) = clocked_host(b_clock.clone()).await;
    let (c, mut c_operator) = clocked_host(c_clock.clone()).await;
    let sender = AttachedDaemon::start(vec![b.peer(B), c.peer(C)]).await;
    let mut writer =
        AttachedClient::attach(&sender.path, ConnectionTier::TrustedLocal, false).await;

    // B: replies at 90, 4 and 25 minutes (journal order unsorted); acks at 10 and 2.
    exchange(
        &mut writer,
        "b-consult-1",
        B,
        SemanticMessageType::Consultation,
        &b,
        &mut b_operator,
        &b_clock,
        &[(10, Act::Acknowledge), (90, Act::Reply)],
    )
    .await;
    exchange(
        &mut writer,
        "b-consult-2",
        B,
        SemanticMessageType::Consultation,
        &b,
        &mut b_operator,
        &b_clock,
        &[(2, Act::Acknowledge), (4, Act::Reply)],
    )
    .await;
    exchange(
        &mut writer,
        "b-bug",
        B,
        SemanticMessageType::BugReport,
        &b,
        &mut b_operator,
        &b_clock,
        &[(25, Act::Reply)],
    )
    .await;
    // C: one reply at 7 minutes (to be left out); acks at 130 and 60.
    exchange(
        &mut writer,
        "c-consult",
        C,
        SemanticMessageType::Consultation,
        &c,
        &mut c_operator,
        &c_clock,
        &[(7, Act::Reply), (130, Act::Acknowledge)],
    )
    .await;
    exchange(
        &mut writer,
        "c-bug",
        C,
        SemanticMessageType::BugReport,
        &c,
        &mut c_operator,
        &c_clock,
        &[(60, Act::Acknowledge)],
    )
    .await;
    // Every recorded act precedes the request's arrival on each host clock.
    b_clock.advance(Duration::from_secs(60));
    c_clock.advance(Duration::from_secs(60));

    let outcomes = send(
        &mut writer,
        "retro",
        &[B, C],
        SemanticMessageType::RetroRequest,
        "Sprint 7 retro",
    )
    .await;
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, TeamSendOutcome::Delivered))
            && outcomes.len() == 2,
        "{outcomes:?}"
    );

    let b_own = json!({
        "received": {"consultation": 2, "bug_report": 1},
        "replyTimes": [4, 25, 90],
        "ackTimes": [2, 10],
    });
    let c_own = json!({
        "received": {"consultation": 1, "bug_report": 1},
        "replyTimes": [7],
        "ackTimes": [60, 130],
    });
    let c_shared = json!({
        "received": {"consultation": 1, "bug_report": 1},
        "ackTimes": [60, 130],
    });
    let mut retro_items = Vec::new();
    for (host, operator, own, leave_out, shared) in [
        (&b, &mut b_operator, &b_own, Vec::new(), &b_own),
        (
            &c,
            &mut c_operator,
            &c_own,
            vec![RetroField::ReplyTimes],
            &c_shared,
        ),
    ] {
        let item = host_item(host, SemanticMessageType::RetroRequest).await;
        let control = wait_host_control(operator, &item).await;
        let preview = serde_json::to_value(&control).unwrap()["retro"].clone();
        assert_eq!(
            preview["contribution"], *own,
            "persisted preview: {preview}"
        );
        assert_eq!(preview["nothingInWindow"], false, "{preview}");

        let outcome = act(operator, &item, leave_out, false).await;
        assert!(
            matches!(&outcome, RecipientActionOutcome::Applied { reply: Some(receipt) } if receipt.kind.wire_name() == "retro" && receipt.item_id == item.item().as_str()),
            "{outcome:?}"
        );

        let journal = replies(host, &item).await;
        assert_eq!(journal.len(), 1, "one Retro turn per item: {journal:?}");
        let turn = &journal[0];
        assert_eq!(turn["kind"], json!({"type": "retro"}), "{turn}");
        assert_eq!(turn["retro"], *shared, "{turn}");
        assert_eq!(turn["content"], RETRO_TEXT, "{turn}");
        assert_eq!(turn["authorship"], "agent-composed", "{turn}");
        assert_eq!(turn["send_authorization"], "human-decided", "{turn}");
        retro_items.push(item);
    }
    let c_journal = replies(&c, &retro_items[1]).await;
    assert!(
        c_journal[0]["retro"].get("replyTimes").is_none(),
        "left-out field still stored: {c_journal:?}"
    );
    assert!(
        replies(&b, &retro_items[0]).await[0]["retro"]
            .get("replyTimes")
            .is_some(),
        "positive control: B shared its reply times"
    );

    let report = board(&mut writer, "report").await;
    assert_eq!(b.method_count("x-rustain-items/thread"), 1, "{report}");
    assert_eq!(c.method_count("x-rustain-items/thread"), 1, "{report}");
    let lines = report.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.first(),
        Some(&"Retrospective report · retro_request"),
        "{report}"
    );
    assert!(
        lines
            .get(1)
            .is_some_and(|line| line.starts_with("attempted ")),
        "{report}"
    );
    // Hand-summed: consultation 2+1, bug_report 1+1; replies [4,25,90] (C left
    // out [7]) → lower median 25m; acks [2,10]+[60,130] → lower median 10m.
    assert_eq!(
        lines[2..],
        [
            "  Received items (14 days): consultation 3 · bug_report 2",
            "  Time to first reply: median 25m · 3 replies",
            "  Time to acknowledge: median 10m · 4 acknowledgements",
            "This report contains only what was consented. It does not report who contributed, or how many.",
        ],
        "{report}"
    );
    for forbidden in [B, C, "addressed", "private", "Nothing has been shared"] {
        assert!(!report.contains(forbidden), "{forbidden}: {report}");
    }

    let observed = sender
        .journal
        .entries()
        .await
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(RoomEvent::PeerReplyObserved {
                answers, content, ..
            }) if retro_items
                .iter()
                .any(|item| answers.as_deref() == Some(item.item().as_str())) =>
            {
                Some(content)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        vec![Some(RETRO_TEXT.to_owned()), Some(RETRO_TEXT.to_owned())],
        "the requester records only the canonical text"
    );

    for (host, item, shared) in [
        (&b, &retro_items[0], &b_own),
        (&c, &retro_items[1], &c_shared),
    ] {
        let thread = served_thread(host, item).await;
        let turns = thread["result"]["turns"]
            .as_array()
            .unwrap_or_else(|| panic!("served turns: {thread}"))
            .iter()
            .filter(|turn| turn["kind"] == "reply" && turn["inReplyTo"] == item.item().as_str())
            .collect::<Vec<_>>();
        assert_eq!(turns.len(), 1, "{thread}");
        let served = turns[0];
        assert_eq!(served["replyKind"], "retro", "{served}");
        assert_eq!(served["retro"], *shared, "{served}");
        assert_eq!(served["content"], RETRO_TEXT, "{served}");
        assert_eq!(served["authorship"], "agent-composed", "{served}");
        assert_no_local_facts(&serde_json::to_string(served).unwrap(), "served turn");
    }

    assert!(b.requests.lock().is_empty(), "B provider requests");
    assert!(c.requests.lock().is_empty(), "C provider requests");
    writer.detach().await;
    b_operator.detach().await;
    c_operator.detach().await;
    sender.stop().await;
    b.stop().await;
    c.stop().await;
}
