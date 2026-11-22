//! Story `agents-thread` `AK02` (AC1, AC3(a), AC3(e)) — rows that share a label and a row
//! that is not this host's dispatch.
//!
//! Two delegations to the real rustain host B are two `a2a-peer` rows; a third-party
//! `wiremock` server that answers `message/send` with a task id of its own (≠ the `messageId`)
//! is a third `a2a-peer` row. Offers and opens are keyed by `AgentId`, never by the label.
//! `M2` (key offers by label) → RED here; `M1` (drop the own-dispatch check) is isolated by the
//! owner's unit table (`agent_thread.rs`). AT-02: a peer that answers a delegation with an
//! OLDER own task id — a previous typed `/team send`'s — names an exchange that is not that
//! delegation's: no offer, and `t` on it performs zero requests.

#![cfg(feature = "test-instrumentation")]

use std::collections::BTreeMap;

use crossterm::event::KeyCode;
use rustain::adapters::tui::mission_control;
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::domain::models::{OwnershipKind, SentThreadOffer};
use rustain::infrastructure::runtime::agents_thread::{self, UNVERIFIABLE};

use super::ak01_open_one::{
    B_TOOL, CONV, ItemsPeer, SendAnswer, Session, delegate, last_notice, press, rustain_peer,
    select, sent_item, show_agents,
};
use super::b_k01_model_park::{SKILL, model_rail};
use super::*;

/// `AK02` — each of two same-label delegation rows opens its own item on B; the third-party
/// row has no offer, and `t`'s effect on it performs zero requests and says AC3(e).
#[tokio::test]
async fn ak02_offers_are_keyed_by_agent_id_and_a_foreign_task_is_never_offered() {
    let b = rustain_peer().await;
    let w = ItemsPeer::start("w", [SendAnswer::Minted]).await;
    let rail = model_rail(vec![b.host.peer("b"), w.spec()]).await;
    let (first, first_task) = delegate(&rail, "b", B_TOOL).await;
    b.quiesce().await;
    let (second, second_task) = delegate(&rail, "b", B_TOOL).await;
    b.quiesce().await;
    let (foreign, _) = delegate(&rail, "w", SKILL).await;
    let mut session = Session::over(&rail);
    let mut state = TuiState::new(100, 30);
    show_agents(&mut state, &rail, &session.app).await;

    let labels: Vec<&str> = state
        .agent_panel_state
        .cached_entries
        .iter()
        .filter(|entry| matches!(entry.ownership, OwnershipKind::Peer))
        .map(|entry| entry.subagent_type.as_str())
        .collect();
    assert_eq!(labels, ["a2a-peer"; 3], "three peer rows, one label");
    let offer = |task: &str| SentThreadOffer {
        peer: "b".to_owned(),
        task: task.to_owned(),
    };
    assert_eq!(
        state.agent_threads.offers,
        BTreeMap::from([
            (first.clone(), offer(&first_task)),
            (second.clone(), offer(&second_task)),
        ]),
        "one offer per own delegation row, none for the foreign task"
    );

    let mut opened = Vec::new();
    for (row, task) in [(&first, &first_task), (&second, &second_task)] {
        select(&mut state, row);
        agents_thread::open(&mut state, CONV, &session.app, row.clone());
        assert!(session.next_ready().await.apply(&mut state));
        let item = b.item_of(task).await;
        assert_eq!(sent_item(&state), Some(("b".to_owned(), item.clone())));
        opened.push(item);
        assert_eq!(press(&mut state, KeyCode::Esc), InputAction::Consumed);
        assert!(mission_control::is_top(&state));
    }
    assert_ne!(opened[0], opened[1], "each row opens its own exchange");

    select(&mut state, &foreign);
    let requests = w.request_count();
    let served = b.served_rows();
    let notices = state.notifications.len();
    agents_thread::open(&mut state, CONV, &session.app, foreign.clone());
    assert_eq!(
        last_notice(&state),
        Some((NoticeSeverity::Warning, UNVERIFIABLE.to_owned()))
    );
    assert_eq!(state.notifications.len(), notices + 1);
    assert!(state.agent_threads.opening.is_none());
    assert!(state.thread_inspector.is_none());
    tokio::task::yield_now().await;
    assert!(session.quiet(), "no lookup was spawned");
    assert_eq!(w.request_count(), requests, "zero requests of any method");
    assert_eq!(b.served_rows(), served);
    b.stop();
}

/// `AK02` (AT-02) — replay of a previous valid own task. A's typed `/team send` to `w`
/// dispatched `T0` (an own dispatch, no delegation node); a later model delegation to `w`
/// submits `T1` and `w` answers with task id `T0`. The delegation's row names `T0`, a task A
/// really dispatched to `w`, yet it is not this delegation's exchange: the production refresh
/// offers nothing, and `t`'s effect says AC3(e) with zero requests of any method.
#[tokio::test]
async fn ak02_a_peer_replaying_an_older_own_task_offers_and_opens_nothing() {
    let w = ItemsPeer::start("w", [SendAnswer::Item("ri_typed"), SendAnswer::Replay]).await;
    let rail = model_rail(vec![w.spec()]).await;
    let mut session = Session::over(&rail);
    let mut state = TuiState::new(100, 30);
    session
        .type_team(&mut state, "/team send w typed first")
        .await;
    session.settle_send(&mut state).await;
    let typed_task = w.first_message();

    let (row, delegated_task) = delegate(&rail, "w", SKILL).await;
    assert_ne!(
        delegated_task, typed_task,
        "the delegation submitted its own messageId"
    );
    let replayed = w
        .requests_for("message/send")
        .last()
        .map(|request| request["params"]["message"]["messageId"].clone());
    assert_eq!(
        replayed.as_ref().and_then(|id| id.as_str()),
        Some(delegated_task.as_str())
    );
    {
        use base64::Engine;
        let b64 = |text: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
        assert_eq!(
            row.as_str(),
            format!("a2a/p-{}/t-{}", b64("w"), b64(&typed_task)),
            "the row names the older typed task"
        );
    }

    show_agents(&mut state, &rail, &session.app).await;
    assert!(
        state
            .agent_panel_state
            .cached_entries
            .iter()
            .any(|entry| entry.agent_id == row),
        "the replayed delegation is a row of the Agents tab"
    );
    assert!(
        state.agent_threads.offers.is_empty(),
        "a replayed own task is not this delegation's exchange: {:?}",
        state.agent_threads.offers
    );

    select(&mut state, &row);
    let requests = w.request_count();
    let notices = state.notifications.len();
    agents_thread::open(&mut state, CONV, &session.app, row.clone());
    assert_eq!(
        last_notice(&state),
        Some((NoticeSeverity::Warning, UNVERIFIABLE.to_owned()))
    );
    assert_eq!(state.notifications.len(), notices + 1);
    assert!(state.agent_threads.opening.is_none());
    assert!(state.thread_inspector.is_none());
    tokio::task::yield_now().await;
    assert!(session.quiet(), "no lookup was spawned");
    assert_eq!(w.request_count(), requests, "zero requests of any method");
}
