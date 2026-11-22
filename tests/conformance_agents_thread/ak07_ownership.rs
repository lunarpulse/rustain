//! Story `agents-thread` `AK07` (AC8 D8, AC3(e), AC4) — authority between open and send.
//!
//! (a) a read-only tab at send and (b) a frame retargeted to another thread before
//! `follow_up_sent` send nothing and keep the text. (c) A delegation row that leaves A's tree
//! after its thread opened: the frame keeps its own `(peer, item)` and target, `t` on the gone
//! row is AC3(e) with no I/O, and the frame's follow-up still sends exactly once — its target
//! comes from the frame, never re-derived from the (gone) Agents row. `M17` (skip the
//! read-only check) and `M18` (re-derive from the row) → RED.

#![cfg(feature = "test-instrumentation")]

use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::infrastructure::runtime::agents_thread::{self, UNVERIFIABLE};
use rustain::infrastructure::runtime::mission_bridge::READ_ONLY;
use rustain::infrastructure::runtime::transparency_bridge::{FOLLOW_UP_CHANGED, follow_up_sent};

use super::ak01_open_one::{
    B_TOOL, CONV, Peer, Session, TEXT, delegate, dispatches, field_text, follow_up, last_notice,
    outcome_lines, refresh, rustain_peer, select, sent_item, show_agents, write_follow_up,
};
use super::b_k01_model_park::model_rail;
use super::*;

/// `(items B received, dispatch rows A wrote, send blocks)`: what a sent follow-up moves.
async fn sends(b: &Peer, sender: &Sender, state: &TuiState) -> (usize, usize, usize) {
    (
        received(&room_events(&b.host.journal).await).len(),
        dispatches(&room_events(&sender.journal).await).len(),
        state.feedback_blocks.len(),
    )
}

/// `AK07` (a) + (b) — the dispatch re-checks the tab and the frame at the moment of sending.
#[tokio::test]
async fn ak07_read_only_and_a_retargeted_frame_send_nothing() {
    let b = rustain_peer().await;
    let mut sender = new_sender(vec![b.host.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (first, _) = ask(&mut sender, &mut state, &b.host, "first").await;
    let (second, _) = ask(&mut sender, &mut state, &b.host, "second").await;
    read_thread_block(&mut sender, &mut state, "b", &first).await;
    assert_eq!(follow_up(&state), Some(first.clone()));
    let InputAction::ThreadFollowUp {
        peer,
        item,
        turn,
        text,
    } = write_follow_up(&mut state, TEXT)
    else {
        panic!("Enter in a follow-up field with text asks to send it");
    };
    let before = sends(&b, &sender, &state).await;

    // (a) The active tab is read-only at the moment of sending.
    follow_up_sent(
        &mut state,
        CONV,
        &peer,
        &item,
        &turn,
        text.clone(),
        true,
        &sender.app_state,
    )
    .await;
    assert_eq!(
        last_notice(&state),
        Some((NoticeSeverity::Warning, READ_ONLY.to_owned()))
    );
    assert_eq!(field_text(&state).as_deref(), Some(TEXT), "the text stays");
    assert_eq!(sends(&b, &sender, &state).await, before, "nothing was sent");
    assert!(state.thread_follow_ups.is_empty());

    // (b) The frame was retargeted to another thread before the send.
    read_thread_block(&mut sender, &mut state, "b", &second).await;
    assert_eq!(sent_item(&state), Some(("b".to_owned(), second.clone())));
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
    let key = format!("sent:b:{first}");
    assert_eq!(
        outcome_lines(&state, &key).last().map(String::as_str),
        Some(FOLLOW_UP_CHANGED)
    );
    assert_eq!(state.thread_outcomes.retained(&key), Some(TEXT));
    assert_eq!(field_text(&state), None, "the new thread has no field");
    assert_eq!(sends(&b, &sender, &state).await, before, "nothing was sent");
    assert!(state.thread_follow_ups.is_empty());
    assert!(sender.domain_rx.try_recv().is_err(), "nothing was started");
    b.stop();
}

/// `AK07` (c) — the delegation row is deregistered after its thread opened; the frame still
/// follows up, once, on its own target.
#[tokio::test]
async fn ak07_a_row_that_left_the_tree_leaves_the_open_frame_whole() {
    let b = rustain_peer().await;
    let rail = model_rail(vec![b.host.peer("b")]).await;
    let (row, task) = delegate(&rail, "b", B_TOOL).await;
    b.quiesce().await;
    let item = b.item_of(&task).await;
    let mut session = Session::over(&rail);
    let mut state = TuiState::new(100, 30);
    show_agents(&mut state, &rail, &session.app).await;
    select(&mut state, &row);
    agents_thread::open(&mut state, CONV, &session.app, row.clone());
    assert!(session.next_ready().await.apply(&mut state));
    assert_eq!(follow_up(&state), Some(item.clone()));
    let content = state.thread_inspector.as_ref().map(|f| f.content.clone());

    rail.tree.deregister(&row).await;
    refresh(&mut state, &rail, &session.app).await;
    assert!(state.agent_threads.offers.is_empty());
    assert_eq!(sent_item(&state), Some(("b".to_owned(), item.clone())));
    assert_eq!(
        state.thread_inspector.as_ref().map(|f| &f.content),
        content.as_ref()
    );
    assert_eq!(
        follow_up(&state),
        Some(item.clone()),
        "the frame keeps its target"
    );

    // `t` on the gone row: AC3(e), nothing requested.
    let served = b.served_rows();
    agents_thread::open(&mut state, CONV, &session.app, row.clone());
    assert_eq!(
        last_notice(&state),
        Some((NoticeSeverity::Warning, UNVERIFIABLE.to_owned()))
    );
    assert!(state.agent_threads.opening.is_none());
    tokio::task::yield_now().await;
    assert!(session.quiet(), "no lookup was spawned");
    assert_eq!(b.served_rows(), served);

    // The frame's follow-up: one send, on the frame's own payload.
    let InputAction::ThreadFollowUp {
        peer,
        item: of,
        turn,
        text,
    } = write_follow_up(&mut state, TEXT)
    else {
        panic!("Enter in a follow-up field with text asks to send it");
    };
    assert_eq!(
        (peer.as_str(), of.as_str(), turn.as_str()),
        ("b", item.as_str(), item.as_str())
    );
    let b_rows = room_events(&b.host.journal).await.len();
    let a_rows = rail.rows().await.len();
    follow_up_sent(
        &mut state,
        CONV,
        &peer,
        &of,
        &turn,
        text,
        false,
        &session.app,
    )
    .await;
    session.settle_send(&mut state).await;
    let key = format!("sent:b:{item}");
    assert_eq!(
        outcome_lines(&state, &key).last().map(String::as_str),
        Some(format!("follow-up answering {item} · Send · ● b delivered").as_str())
    );
    let reply_to: Vec<_> = room_events(&b.host.journal)
        .await
        .split_off(b_rows)
        .into_iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RecipientItemReceived { in_reply_to, .. } => Some(in_reply_to),
            _ => None,
        })
        .collect();
    assert_eq!(
        reply_to,
        vec![Some(item.clone())],
        "the frame's item is the reply-to"
    );
    assert_eq!(
        dispatches(&rail.rows().await.split_off(a_rows)).len(),
        1,
        "one dispatch row"
    );
    b.stop();
}
