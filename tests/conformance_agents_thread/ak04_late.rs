//! Story `agents-thread` `AK04` (AC5) — late and superseded completions are dropped.
//!
//! The hold is structural: `#[tokio::test]` runs on a current-thread runtime, so the lookup
//! `agents_thread::open` spawns cannot run (let alone reach B) before the test next awaits; the
//! operator's move therefore lands strictly before the read completes, and the completion is
//! then taken from A's bus and applied by the production arm. `M10` (drop the generation /
//! selection guard) → RED: a frame opens late. AT-05: a completion dropped because a frame
//! covers Mission Control still ends its pending lookup, so `t` works again once uncovered.

#![cfg(feature = "test-instrumentation")]

use crossterm::event::KeyCode;
use rustain::adapters::tui::mission_control;
use rustain::adapters::tui::presentation::{self, Surface, reducer};
use rustain::infrastructure::runtime::agents_thread;

use super::ak01_open_one::{
    B_TOOL, CONV, Session, delegate, press, rustain_peer, select, sent_item, show_agents,
};
use super::b_k01_model_park::model_rail;
use super::*;

/// `AK04` — the selection moved, Mission Control closed, and `t` pressed on another row: in
/// each case the earlier lookup's completion opens no frame, posts no notice and changes
/// nothing; only the newest lookup of the selected row applies.
#[tokio::test]
async fn ak04_a_completion_that_is_no_longer_pending_applies_nothing() {
    let b = rustain_peer().await;
    let rail = model_rail(vec![b.host.peer("b")]).await;
    let (first, _) = delegate(&rail, "b", B_TOOL).await;
    b.quiesce().await;
    let (second, second_task) = delegate(&rail, "b", B_TOOL).await;
    b.quiesce().await;
    let mut session = Session::over(&rail);
    let mut state = TuiState::new(100, 30);
    show_agents(&mut state, &rail, &session.app).await;
    let notices = state.notifications.len();

    // (1) The selection moves off the row before its read completes.
    select(&mut state, &first);
    agents_thread::open(&mut state, CONV, &session.app, first.clone());
    select(&mut state, &second);
    assert!(state.agent_threads.opening.is_none(), "voided at once");
    assert!(!session.next_ready().await.apply(&mut state));
    assert!(state.thread_inspector.is_none());
    assert_eq!(state.notifications.len(), notices);
    assert_eq!(state.mission_control.selected_id(), Some(second.as_str()));

    // (2) Mission Control closes before the read completes.
    select(&mut state, &first);
    agents_thread::open(&mut state, CONV, &session.app, first.clone());
    assert_eq!(press(&mut state, KeyCode::Esc), InputAction::Consumed);
    assert!(!presentation::is_open(&state, Surface::MissionControl));
    assert!(state.agent_threads.opening.is_none());
    assert!(!session.next_ready().await.apply(&mut state));
    assert!(state.thread_inspector.is_none());
    assert!(!presentation::is_open(&state, Surface::MissionControl));
    assert_eq!(state.notifications.len(), notices);

    // (3) `t` on another row supersedes the first lookup: only the newest one applies.
    show_agents(&mut state, &rail, &session.app).await;
    select(&mut state, &first);
    agents_thread::open(&mut state, CONV, &session.app, first.clone());
    select(&mut state, &second);
    agents_thread::open(&mut state, CONV, &session.app, second.clone());
    let mut applied = Vec::new();
    for _ in 0..2 {
        let ready = session.next_ready().await;
        let agent = ready.agent_id.clone();
        if ready.apply(&mut state) {
            applied.push(agent);
        }
    }
    assert_eq!(applied, vec![second.clone()], "only the newest applies");
    let item = b.item_of(&second_task).await;
    assert_eq!(sent_item(&state), Some(("b".to_owned(), item)));
    assert!(state.agent_threads.opening.is_none());
    assert_eq!(state.notifications.len(), notices);
    assert_eq!(press(&mut state, KeyCode::Esc), InputAction::Consumed);
    assert!(mission_control::is_top(&state));
    assert_eq!(state.mission_control.selected_id(), Some(second.as_str()));

    // (4) AT-05: Help covers Mission Control before the read completes. The completion is
    // dropped (no frame, no notice) but ends the pending lookup; uncovered, `t` opens again.
    agents_thread::open(&mut state, CONV, &session.app, second.clone());
    assert!(state.agent_threads.opening.is_some());
    let _ = reducer::open(&mut state, Surface::Help);
    assert!(!mission_control::is_top(&state));
    assert!(!session.next_ready().await.apply(&mut state));
    assert!(!presentation::is_open(&state, Surface::ThreadInspector));
    assert_eq!(state.notifications.len(), notices);
    assert!(
        state.agent_threads.opening.is_none(),
        "the dropped completion ended its lookup"
    );
    let _ = reducer::back(&mut state);
    assert!(mission_control::is_top(&state));
    assert_eq!(state.mission_control.selected_id(), Some(second.as_str()));
    agents_thread::open(&mut state, CONV, &session.app, second.clone());
    assert!(session.next_ready().await.apply(&mut state));
    let item = b.item_of(&second_task).await;
    assert_eq!(sent_item(&state), Some(("b".to_owned(), item)));
    assert_eq!(state.notifications.len(), notices);
    b.stop();
}
