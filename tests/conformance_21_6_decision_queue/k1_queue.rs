//! K1 — the queue store and the Needs-you chip (AC1, AC5; A2, A11).
//!
//! The items are raised through their real producers (the context-warning
//! handler, the sidebar delete front door, the paste path) and read back through
//! what the user sees: the chip on the real status row and the frame.

use rustain::adapters::tui::decision_queue::{
    self, DecisionKind, DecisionSubject, Validity, waiting,
};
use rustain::adapters::tui::handlers::context_warning::{
    handle_clear_context_warning, handle_upsert_context_warning,
};
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface, is_open};
use rustain::domain::events::DomainKey;
use rustain::domain::models::ImageAttachment;
use rustain::domain::models::visual::DeleteConfirmTarget;
use rustain::infrastructure::runtime::event_loop::begin_delete_confirmation;

use super::support::{clocked, facts, ready, route, special, status_row};

fn image() -> ImageAttachment {
    ImageAttachment {
        media_type: "image/png".into(),
        data: "AAAA".into(),
    }
}

fn kinds(state: &rustain::adapters::tui::state::TuiState) -> Vec<(DecisionKind, Option<String>)> {
    state
        .decisions
        .items()
        .map(|item| (item.kind, item.conversation_id.clone()))
        .collect()
}

#[test]
fn k1_the_chip_counts_every_queued_item_and_a_producer_update_never_adds_one() {
    let (mut state, _clock) = clocked(80, 24);
    handle_upsert_context_warning(&mut state, "c1", 85);
    handle_upsert_context_warning(&mut state, "c2", 90);
    decision_queue::raise_large_image(&mut state, image(), "Image is large (6 MB).");
    assert_eq!(waiting(&state), 3);
    assert!(
        status_row(&state, 80).contains("Needs you (3) ^X q"),
        "{}",
        status_row(&state, 80)
    );
    // The same producer for the same conversation updates in place: still 3.
    handle_upsert_context_warning(&mut state, "c2", 92);
    assert_eq!(waiting(&state), 3);
    let c2 = state
        .decisions
        .items()
        .find(|item| item.conversation_id.as_deref() == Some("c2"))
        .expect("c2's item");
    assert_eq!(c2.detail, "Running low on context (92%).");
    // Positive control: an empty queue paints no chip at all.
    let (empty, _clock) = clocked(80, 24);
    assert!(!status_row(&empty, 80).contains("Needs you"));
}

#[test]
fn k1_a_clear_drops_only_its_own_conversation_and_a_closed_tab_is_pruned() {
    let (mut state, clock) = clocked(80, 24);
    let f = facts("c1");
    handle_upsert_context_warning(&mut state, "c1", 85);
    handle_upsert_context_warning(&mut state, "c2", 90);
    // The user sets c1's frame aside, then its condition clears elsewhere.
    ready(&mut state, &clock);
    route(&mut state, &special(DomainKey::Esc), &f);
    assert!(!is_open(&state, Surface::Decision));
    handle_clear_context_warning(&mut state, "c1");
    assert_eq!(
        kinds(&state),
        vec![(DecisionKind::ContextWarning, Some("c2".into()))]
    );
    assert!(status_row(&state, 80).contains("Needs you (1) ^X q"));
    // Tab c2 closes: its item can never be decided, so it stops counting.
    let only_c1 = LogFacts {
        titles: vec![("c1".into(), "one".into())],
        ..facts("c1")
    };
    decision_queue::prune_closed(&mut state, &only_c1);
    assert!(state.decisions.is_empty());
    assert!(!status_row(&state, 80).contains("Needs you"));
}

#[test]
fn k1_the_sidebar_delete_creates_exactly_one_item_that_owns_its_target() {
    let (mut state, clock) = clocked(80, 24);
    let f = facts("c1");
    let target = DeleteConfirmTarget::Single {
        id: "c9".into(),
        title: "Old chat".into(),
    };
    begin_delete_confirmation(&mut state, target.clone());
    begin_delete_confirmation(&mut state, target.clone());
    assert_eq!(waiting(&state), 1, "a second request updates in place");
    let item = state.decisions.items().next().unwrap();
    assert_eq!(item.kind, DecisionKind::DeleteConversations);
    assert_eq!(item.subject, DecisionSubject::Delete(target));
    assert_eq!(item.validity, Validity::Live);
    assert!(presentation::is_open(&state, Surface::Decision));
    // Esc is the legacy cancel: the item leaves, nothing was deleted.
    ready(&mut state, &clock);
    let effect = route(&mut state, &special(DomainKey::Esc), &f);
    assert!(matches!(effect, Some(RouteEffect::Action(_))), "{effect:?}");
    assert!(state.decisions.is_empty());
}
