//! Story `agents-thread` `AK03` (AC3(b), AC3(c), AC3(d)) — exact binding only.
//!
//! A `wiremock` peer that echoes the `messageId` as its task id (a rustain host's shape, so the
//! row is an own dispatch and offered) and serves `x-rustain-items/list` rows shaped like
//! `server.rs` `items_list` (`{itemId, task, state, ordinal}`). Zero rows for `T`, two rows for
//! `T` and a failed list each open nothing, read no thread and post one exact Warning.
//! `M6` (pick the first of several rows) → RED here.

#![cfg(feature = "test-instrumentation")]

use std::collections::BTreeMap;

use rustain::adapters::tui::mission_control;
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::domain::models::SentThreadOffer;
use rustain::infrastructure::runtime::agents_thread;

use super::ak01_open_one::{
    CONV, ItemsPeer, List, SendAnswer, Session, delegate, last_notice, select, show_agents,
};
use super::b_k01_model_park::{SKILL, model_rail};
use super::*;

/// `AK03` — no item, two items and an HTTP 500 list: no frame, no thread read, the AC3
/// sentences verbatim (the failure in the existing outcome words).
#[tokio::test]
async fn ak03_zero_or_several_items_or_a_failed_list_open_nothing() {
    let o = ItemsPeer::start("o", [SendAnswer::Echo]).await;
    let rail = model_rail(vec![o.spec()]).await;
    let (row, task) = delegate(&rail, "o", SKILL).await;
    assert_eq!(task, o.first_message(), "T is the own messageId");
    let mut session = Session::over(&rail);
    let mut state = TuiState::new(100, 30);
    show_agents(&mut state, &rail, &session.app).await;
    assert_eq!(
        state.agent_threads.offers,
        BTreeMap::from([(
            row.clone(),
            SentThreadOffer {
                peer: "o".to_owned(),
                task,
            },
        )])
    );
    select(&mut state, &row);
    let requests = o.request_count();

    let legs = [
        (
            List::Rows(vec![("ri_other", Some("someone-elses-task"))]),
            "o's host lists no item for this agent's task — nothing was opened",
        ),
        (
            List::Rows(vec![("ri_a", None), ("ri_b", None)]),
            "o's host lists 2 items for this agent's task: ri_a, ri_b — open one with /team thread o <item-id>",
        ),
        (
            List::Http(500),
            "could not read o's items — no usable answer — '/team board' shows whether it arrived",
        ),
    ];
    for (list, sentence) in legs {
        o.list(list);
        let lists = o.requests_for("x-rustain-items/list").len();
        agents_thread::open(&mut state, CONV, &session.app, row.clone());
        assert!(session.next_ready().await.apply(&mut state));
        assert_eq!(
            last_notice(&state),
            Some((NoticeSeverity::Warning, sentence.to_owned()))
        );
        assert!(state.thread_inspector.is_none(), "no frame for {sentence}");
        assert!(mission_control::is_top(&state));
        assert!(state.agent_threads.opening.is_none());
        assert_eq!(
            o.requests_for("x-rustain-items/list").len(),
            lists + 1,
            "one scoped list read"
        );
    }
    assert!(
        o.requests_for("x-rustain-items/thread").is_empty(),
        "no thread is read unless exactly one item names the task"
    );
    assert_eq!(
        o.request_count(),
        requests + 3,
        "three list reads and nothing else: no message/send, no tasks/*"
    );
}
