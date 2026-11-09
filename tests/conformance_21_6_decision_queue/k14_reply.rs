//! K14: reply arrivals are pull-only and do not preempt the current surface.

use super::support::{clocked, facts, key, paint, ready, route, screen, special};
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::decision_queue::{self, DecisionEffect, reply};
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface};
use rustain::domain::events::DomainKey;
use std::collections::HashMap;
use std::time::Duration;

#[test]
fn story21_6_reply_pull_deduplicates_and_journal_read_withdraws() {
    let (mut state, clock) = clocked(80, 24);
    presentation::reducer::open(&mut state, Surface::Help);
    reply::observe(&mut state, "peer-principal", "roster-alias", "ri_1", 1);
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
    assert_eq!(decision_queue::waiting(&state), 1);
    let notice = state
        .notifications
        .records()
        .find(|r| r.message == "roster-alias replied")
        .unwrap();
    assert!(notice.action.is_none());
    assert_eq!(notice.count, 1);
    reply::observe(&mut state, "peer-principal", "roster-alias", "ri_1", 1);
    assert_eq!(
        state
            .notifications
            .records()
            .find(|r| r.message == "roster-alias replied")
            .unwrap()
            .count,
        1
    );
    clock.advance(Duration::from_secs(600));
    assert_eq!(decision_queue::waiting(&state), 1);
    reply::journal_counts(
        &mut state,
        &HashMap::from([(("peer-principal".into(), "ri_1".into()), 1)]),
    );
    assert_eq!(decision_queue::waiting(&state), 0);
}

#[test]
fn story21_6_reply_leader_lists_and_enter_returns_read_effect() {
    let (mut state, clock) = clocked(80, 24);
    state.input_buffer = "composer retained".into();
    reply::observe(&mut state, "principal", "Alice", "ri_1", 2);
    let _ = handle_input(&mut state, &special(DomainKey::CtrlX));
    assert_eq!(
        handle_input(&mut state, &key('q')),
        InputAction::FocusDecisions
    );
    decision_queue::open_waiting(&mut state, &facts("other-tab"));
    assert!(screen(&paint(&mut state, &facts("other-tab"), 80, 24)).contains("Alice"));
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("other-tab"));
    ready(&mut state, &clock);
    assert_eq!(
        route(&mut state, &special(DomainKey::Enter), &facts("other-tab")),
        Some(RouteEffect::Action(InputAction::Decision(
            DecisionEffect::ReplyRead
        )))
    );
    assert_eq!(state.input_buffer, "composer retained");
}

#[test]
fn story21_6_reply_no_event_for_ten_minutes_creates_no_item() {
    let (mut state, clock) = clocked(80, 24);
    clock.advance(Duration::from_secs(600));
    decision_queue::open_waiting(&mut state, &facts("c1"));
    assert_eq!(decision_queue::waiting(&state), 0);
    assert!(!presentation::is_open(&state, Surface::DecisionQueue));
}

#[tokio::test(start_paused = true)]
async fn story21_6_reply_refresh_leaves_no_live_background_task_over_ten_minutes() {
    let (mut state, _) = clocked(80, 24);
    let runtime = tokio::runtime::Handle::current();
    let before = runtime.metrics().num_alive_tasks();
    reply::observe(&mut state, "principal", "Alice", "ri_1", 1);
    assert_eq!(decision_queue::waiting(&state), 1);
    reply::journal_counts(
        &mut state,
        &HashMap::from([(("principal".into(), "ri_1".into()), 1)]),
    );
    reply::refresh(&mut state);
    tokio::task::yield_now().await;
    assert_eq!(runtime.metrics().num_alive_tasks(), before);
    tokio::time::advance(Duration::from_secs(600)).await;
    tokio::task::yield_now().await;
    assert_eq!(runtime.metrics().num_alive_tasks(), before);
    assert_eq!(decision_queue::waiting(&state), 0);
    assert!(!presentation::is_open(&state, Surface::Decision));
    assert!(!presentation::is_open(&state, Surface::DecisionQueue));
}

#[cfg(feature = "a2a")]
#[tokio::test]
async fn story21_6_opening_reply_and_list_makes_no_peer_requests() {
    use axum::{Router, extract::State, http::Method};
    use rustain::adapters::a2a::egress::A2aEgress;
    use rustain::domain::events::AppEvent;
    use rustain::domain::models::{A2aPeerSource, A2aPeerSpec, RedactedUrl};
    use rustain::domain::ports::RoomJournal;
    use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct Counts {
        get: AtomicUsize,
        post: AtomicUsize,
    }
    async fn count(State(counts): State<Arc<Counts>>, method: Method) -> &'static str {
        match method {
            Method::GET => {
                counts.get.fetch_add(1, Ordering::SeqCst);
            }
            Method::POST => {
                counts.post.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        "not an A2A card"
    }
    let counts = Arc::new(Counts::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server_counts = counts.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(count).with_state(server_counts),
        )
        .await
        .unwrap();
    });
    let workspace = tempfile::tempdir().unwrap();
    let journal = Arc::new(NodeJournal::open_workspace(workspace.path()).await.unwrap());
    let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(journal.clone(), None));
    let peer = A2aPeerSpec::new(
        "Alice",
        RedactedUrl::from(endpoint.clone()),
        A2aPeerSource::Workspace,
    );
    let principal = peer.resolved_identity().to_string();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let keys = tempfile::tempdir().unwrap();
    let egress = A2aEgress::compose(
        vec![peer],
        NodeTree::new(),
        room,
        journal,
        tx,
        rustain::adapters::a2a::item_capability::load_item_capability_key(Ok(keys
            .path()
            .to_path_buf())),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(20), egress.await_cards_settled())
        .await
        .unwrap();
    // This GET is the composed egress's actual card discovery, not a separate
    // client request. A direct POST is only a positive control for that counter.
    assert!(counts.get.load(Ordering::SeqCst) > 0);
    reqwest::Client::new().post(&endpoint).send().await.unwrap();
    assert!(counts.post.load(Ordering::SeqCst) > 0);
    counts.get.store(0, Ordering::SeqCst);
    counts.post.store(0, Ordering::SeqCst);

    let (mut state, _) = clocked(80, 24);
    reply::observe(&mut state, &principal, "Alice", "ri_1", 1);
    // These opening functions accept only TuiState; the real egress remains live
    // alongside them, but the event-loop dispatch itself is not exercised here.
    reply::refresh(&mut state);
    decision_queue::open_waiting(&mut state, &facts("c1"));
    assert!(presentation::is_open(&state, Surface::DecisionQueue));
    tokio::task::yield_now().await;
    assert_eq!(
        (
            counts.get.load(Ordering::SeqCst),
            counts.post.load(Ordering::SeqCst)
        ),
        (0, 0)
    );
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    assert!(presentation::is_open(&state, Surface::Decision));
    tokio::task::yield_now().await;
    assert_eq!(
        (
            counts.get.load(Ordering::SeqCst),
            counts.post.load(Ordering::SeqCst)
        ),
        (0, 0)
    );
    server.abort();
}

#[test]
fn story21_6_reply_dismiss_is_stable_until_a_new_count() {
    let (mut state, clock) = clocked(80, 24);
    reply::observe(&mut state, "principal", "Alice", "ri_1", 1);
    decision_queue::open_waiting(&mut state, &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    ready(&mut state, &clock);
    assert_eq!(
        route(&mut state, &key('x'), &facts("c1")),
        Some(RouteEffect::Action(InputAction::Decision(
            DecisionEffect::ReplyDismiss
        )))
    );
    assert_eq!(decision_queue::waiting(&state), 0);
    reply::observe(&mut state, "principal", "Alice", "ri_1", 1);
    assert_eq!(decision_queue::waiting(&state), 0);
    reply::observe(&mut state, "principal", "Alice", "ri_1", 2);
    assert_eq!(decision_queue::waiting(&state), 1);
}
