//! Story `agents-thread` `AK05` (AC6, AC7, AC8, AC9, AC10) — the frame's follow-up is the one
//! human send, and its receipts land in the frame.
//!
//! The headline journey starts from the Agents delegation row: `t`'s effect opens the
//! delegation's own exchange (its dispatch row carries the delegated text's hash, so the item
//! is this host's own turn), then `f`, the text and `Enter` go through the local front door;
//! the action the field returns is handed to `transparency_bridge::follow_up_sent` exactly as
//! the event loop does, and the row settles through the real `AppEvent::TeamSendSettled` →
//! `team_send_settled`. A typed-send variant drives the same field from a typed
//! `/team thread b <item>` frame on a `/team send` item (AC6's other front door).
//! `M15` (the field writes the composer) and `M19` (a second send path) → RED here.

#![cfg(feature = "test-instrumentation")]

use rustain::domain::models::SendAuthorization;
use rustain::infrastructure::runtime::agents_thread;
use rustain::infrastructure::runtime::transparency_bridge::follow_up_sent;

use super::ak01_open_one::{
    B_TOOL, CARET, CONV, DRAFT, Session, TEXT, delegate, dispatches, field_text, follow_up,
    outcome_lines, rustain_peer, select, sent_item, show_agents, write_follow_up,
};
use super::b_k01_model_park::model_rail;
use super::*;

/// `AK05` — the Agents journey: Agents `t` → the delegation's frame → `f` → text → `Enter`.
/// One POST carrying the delegated item as its reply-to reference: B journals exactly one new
/// item linked to it, A exactly one human-written, human-decided dispatch; the frame's log
/// gains `sending…` then `delivered`, the text is cleared, the Main draft and caret are
/// byte-identical throughout.
#[tokio::test]
async fn ak05_the_agents_delegation_row_follows_up_from_its_own_frame() {
    let b = rustain_peer().await;
    let rail = model_rail(vec![b.host.peer("b")]).await;
    let (row, delegated) = delegate(&rail, "b", B_TOOL).await;
    b.quiesce().await;
    let item = b.item_of(&delegated).await;
    let mut session = Session::over(&rail);
    let mut state = TuiState::new(120, 40);
    state.input_buffer = DRAFT.to_owned();
    state.cursor_position = CARET;

    show_agents(&mut state, &rail, &session.app).await;
    select(&mut state, &row);
    agents_thread::open(&mut state, CONV, &session.app, row.clone());
    assert!(session.next_ready().await.apply(&mut state));
    assert_eq!(sent_item(&state), Some(("b".to_owned(), item.clone())));
    assert_eq!(
        follow_up(&state),
        Some(item.clone()),
        "the delegated item is this host's own turn"
    );
    let notices = state.notifications.len();

    let action = write_follow_up(&mut state, TEXT);
    assert_eq!(
        action,
        InputAction::ThreadFollowUp {
            peer: "b".to_owned(),
            item: item.clone(),
            turn: item.clone(),
            text: TEXT.to_owned(),
        }
    );
    assert_eq!(state.input_buffer, DRAFT);
    assert_eq!(state.cursor_position, CARET);
    let InputAction::ThreadFollowUp {
        peer,
        item: of,
        turn,
        text,
    } = action
    else {
        unreachable!("asserted above");
    };
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

    let key = format!("sent:b:{item}");
    let sending = format!("follow-up answering {item} · Send · b sending…");
    assert_eq!(outcome_lines(&state, &key), vec![sending.clone()]);
    assert_eq!(field_text(&state), None, "the field closed");
    let block = session.settle_send(&mut state).await;
    assert_eq!(
        state.feedback_blocks[&block].message,
        "Send · ● b delivered"
    );
    assert_eq!(
        outcome_lines(&state, &key),
        vec![
            sending,
            format!("follow-up answering {item} · Send · ● b delivered"),
        ]
    );
    assert_eq!(
        state.thread_outcomes.retained(&key),
        None,
        "delivered clears the text"
    );
    assert!(state.thread_follow_ups.is_empty());
    assert_eq!(sent_item(&state), Some(("b".to_owned(), item.clone())));

    let linked: Vec<_> = room_events(&b.host.journal)
        .await
        .split_off(b_rows)
        .into_iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RecipientItemReceived {
                task,
                content,
                in_reply_to,
                ..
            } => Some((task, content, in_reply_to)),
            _ => None,
        })
        .collect();
    assert_eq!(linked.len(), 1, "exactly one POST reached B: {linked:?}");
    let (task, content, link) = linked[0].clone();
    assert_eq!(content, TEXT);
    assert_eq!(
        link,
        Some(item.clone()),
        "the delegated item is the reply-to"
    );
    let dispatched = dispatches(&rail.rows().await.split_off(a_rows));
    assert_eq!(dispatched.len(), 1, "one dispatch row: {dispatched:?}");
    assert_eq!(dispatched[0].0, task);
    assert_eq!(dispatched[0].1, Authorship::HumanWritten);
    assert_eq!(dispatched[0].2, SendAuthorization::HumanDecided);
    assert_eq!(session.provider_requests(), 0, "never a model turn");
    assert_eq!(state.input_buffer, DRAFT);
    assert_eq!(state.cursor_position, CARET);
    assert_eq!(state.notifications.len(), notices);
    b.stop();
}

/// `AK05` (typed-send variant) — the same field on a typed `/team thread b <item>` frame of a
/// `/team send` item: one POST carrying the turn as its reply-to reference: B journals exactly
/// one new item linked to it, A exactly one human-written, human-decided dispatch; the frame's
/// log gains `sending…` then `delivered`, the text is cleared, the Main draft is untouched.
#[tokio::test]
async fn ak05_the_follow_up_is_one_human_send_with_its_receipts_in_the_frame() {
    let b = rustain_peer().await;
    let mut sender = new_sender(vec![b.host.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (item, _) = ask(&mut sender, &mut state, &b.host, "question").await;
    read_thread_block(&mut sender, &mut state, "b", &item).await;
    assert_eq!(
        follow_up(&state),
        Some(item.clone()),
        "a /team send item is this host's own turn"
    );
    state.input_buffer = DRAFT.to_owned();
    state.cursor_position = CARET;
    let notices = state.notifications.len();

    let action = write_follow_up(&mut state, TEXT);
    assert_eq!(
        action,
        InputAction::ThreadFollowUp {
            peer: "b".to_owned(),
            item: item.clone(),
            turn: item.clone(),
            text: TEXT.to_owned(),
        }
    );
    let InputAction::ThreadFollowUp {
        peer,
        item: of,
        turn,
        text,
    } = action
    else {
        unreachable!("asserted above");
    };
    let b_rows = room_events(&b.host.journal).await.len();
    let a_rows = room_events(&sender.journal).await.len();
    follow_up_sent(
        &mut state,
        CONV,
        &peer,
        &of,
        &turn,
        text,
        false,
        &sender.app_state,
    )
    .await;

    let key = format!("sent:b:{item}");
    let sending = format!("follow-up answering {item} · Send · b sending…");
    assert_eq!(outcome_lines(&state, &key), vec![sending.clone()]);
    assert_eq!(field_text(&state), None, "the field closed");
    settle_send(&mut sender, &mut state).await;
    assert_eq!(
        state.feedback_blocks["team-send-2"].message, "Send · ● b delivered",
        "the Stage shows the typed verb's block"
    );
    assert_eq!(
        outcome_lines(&state, &key),
        vec![
            sending,
            format!("follow-up answering {item} · Send · ● b delivered"),
        ]
    );
    assert_eq!(
        state.thread_outcomes.retained(&key),
        None,
        "delivered clears the text"
    );
    assert!(state.thread_follow_ups.is_empty());

    let linked: Vec<_> = room_events(&b.host.journal)
        .await
        .split_off(b_rows)
        .into_iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RecipientItemReceived {
                task,
                content,
                in_reply_to,
                ..
            } => Some((task, content, in_reply_to)),
            _ => None,
        })
        .collect();
    assert_eq!(linked.len(), 1, "exactly one POST reached B: {linked:?}");
    let (task, content, link) = linked[0].clone();
    assert_eq!(content, TEXT);
    assert_eq!(link, Some(item.clone()), "the turn is the reply-to");
    let dispatched = dispatches(&room_events(&sender.journal).await.split_off(a_rows));
    assert_eq!(dispatched.len(), 1, "one dispatch row: {dispatched:?}");
    assert_eq!(dispatched[0].0, task);
    assert_eq!(dispatched[0].1, Authorship::HumanWritten);
    assert_eq!(dispatched[0].2, SendAuthorization::HumanDecided);
    assert_eq!(state.input_buffer, DRAFT);
    assert_eq!(state.cursor_position, CARET);
    assert_eq!(state.notifications.len(), notices);
    b.stop();
}
