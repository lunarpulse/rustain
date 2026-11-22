//! Story `agents-thread` `AK06` (AC9) — a lost answer is reported once and never resent.
//!
//! A `wiremock` rustain-shaped peer `w`: `/team send w hello` is delivered as item `ri_w1`,
//! `/team thread w ri_w1` serves that item turn (this host's own: task and content match the
//! dispatch row, so it is the follow-up target), and the follow-up's `message/send` is taken
//! and then answered by nothing usable (a JSON-RPC error after dispatch: `wiremock` cannot cut
//! a connection, and both prove nothing about arrival — `SendErrorClass::Unproven`).
//! `M20` (resend on `NoUsableAnswer`) → RED: a second follow-up POST.

#![cfg(feature = "test-instrumentation")]

use rustain::domain::models::SendAuthorization;
use rustain::infrastructure::runtime::transparency_bridge::follow_up_sent;

use super::ak01_open_one::{
    CONV, ItemsPeer, SendAnswer, TEXT, dispatches, follow_up, outcome_lines, write_follow_up,
};
use super::*;

const ITEM: &str = "ri_w1";
const LOST: &str = "no usable answer — '/team board' shows whether it arrived";

/// `AK06` — exactly one follow-up POST and one dispatch row, the receipt in the existing
/// words, the text retained, and nothing requested or published after settlement.
#[tokio::test]
async fn ak06_a_lost_answer_is_one_post_and_keeps_the_text() {
    let w = ItemsPeer::start("w", [SendAnswer::Item(ITEM), SendAnswer::Lost]).await;
    w.thread(ITEM, "hello");
    let mut sender = new_sender(vec![w.spec()]).await;
    let mut state = TuiState::new(120, 40);
    sender.type_command(&mut state, "/team send w hello").await;
    settle_send(&mut sender, &mut state).await;
    assert_eq!(
        state.feedback_blocks["team-send-1"].message,
        "Send · ● w delivered"
    );
    read_thread_block(&mut sender, &mut state, "w", ITEM).await;
    assert_eq!(follow_up(&state).as_deref(), Some(ITEM));

    let InputAction::ThreadFollowUp {
        peer,
        item,
        turn,
        text,
    } = write_follow_up(&mut state, TEXT)
    else {
        panic!("Enter in a follow-up field with text asks to send it");
    };
    let a_rows = room_events(&sender.journal).await.len();
    follow_up_sent(
        &mut state,
        CONV,
        &peer,
        &item,
        &turn,
        text,
        false,
        &sender.app_state,
    )
    .await;
    settle_send(&mut sender, &mut state).await;

    assert_eq!(
        state.feedback_blocks["team-send-2"].message,
        format!("Send · w {LOST}")
    );
    let key = format!("sent:w:{ITEM}");
    assert_eq!(
        outcome_lines(&state, &key),
        vec![
            format!("follow-up answering {ITEM} · Send · w sending…"),
            format!("follow-up answering {ITEM} · Send · w {LOST}"),
        ]
    );
    assert_eq!(
        state.thread_outcomes.retained(&key),
        Some(TEXT),
        "nothing proves the peer took it"
    );

    let posts = w.requests_for("message/send");
    assert_eq!(posts.len(), 2, "the send, then exactly one follow-up POST");
    let follow = &posts[1]["params"]["message"];
    assert_eq!(
        follow["metadata"][rustain::adapters::a2a::REPLY_TO_METADATA_KEY],
        ITEM
    );
    let rows = room_events(&sender.journal).await.split_off(a_rows);
    assert_eq!(rows.len(), 1, "only the dispatch row: {rows:?}");
    let dispatched = dispatches(&rows);
    assert_eq!(dispatched.len(), 1);
    assert_eq!(follow["messageId"], dispatched[0].0.as_str());
    assert_eq!(dispatched[0].1, Authorship::HumanWritten);
    assert_eq!(dispatched[0].2, SendAuthorization::HumanDecided);

    // No redelivery: the settled send left nothing running that could ask again.
    let seen = w.request_count();
    tokio::task::yield_now().await;
    assert!(
        sender.domain_rx.try_recv().is_err(),
        "nothing further was published"
    );
    assert_eq!(w.request_count(), seen, "no request after settlement");
}
