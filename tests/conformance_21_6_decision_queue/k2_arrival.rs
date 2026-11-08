//! K2 — RU-6 arrival exactly (AC3; A5). One case per protected state, by value,
//! each with its positive control, entered through the real producer
//! (`handle_upsert_context_warning`, `begin_delete_confirmation`) and the real key
//! decoder (`handle_input`). Time is a `MockClock`.

use std::time::Duration;

use rustain::adapters::tui::app::handle_input;
use rustain::adapters::tui::decision_queue::{self, DecisionKind, waiting, waiting_toast};
use rustain::adapters::tui::handlers::context_warning::{
    handle_clear_context_warning, handle_upsert_context_warning,
};
use rustain::adapters::tui::presentation::{self, Surface, is_open};
use rustain::adapters::tui::quit_confirm::{QuitFacts, QuitHost};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::clock::MockClock;
use rustain::domain::events::DomainKey;
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::DeleteConfirmTarget;
use rustain::infrastructure::runtime::event_loop::begin_delete_confirmation;

use super::support::{advance, clocked, facts, key, ready, route, special};

fn toasts(state: &TuiState) -> usize {
    state
        .notifications
        .visible()
        .filter(|r| r.message == waiting_toast())
        .count()
}

type Setup = fn(&mut TuiState, &MockClock);

fn delete_target() -> DeleteConfirmTarget {
    DeleteConfirmTarget::Single {
        id: "c9".into(),
        title: "Old chat".into(),
    }
}

/// The first arrival after `setup`: (case, setup, takes the frame).
fn cases() -> Vec<(&'static str, Setup, bool)> {
    vec![
        ("idle empty Input", |_, _| {}, true),
        ("Chat focus", |s, _| s.focus = FocusState::Chat, true),
        (
            "Help (a non-text L2)",
            |s, _| {
                presentation::reducer::open(s, Surface::Help);
            },
            true,
        ),
        (
            "Input with text",
            |s, _| {
                s.focus = FocusState::Input;
                s.input_buffer = "half a sentence".into();
            },
            false,
        ),
        (
            "a printable key 999 ms ago",
            |s, c| {
                handle_input(s, &key('a'));
                s.input_buffer.clear();
                s.cursor_position = 0;
                advance(c, Duration::from_millis(999));
            },
            false,
        ),
        (
            "a printable key 1,001 ms ago",
            |s, c| {
                handle_input(s, &key('a'));
                s.input_buffer.clear();
                s.cursor_position = 0;
                advance(c, Duration::from_millis(1_001));
            },
            true,
        ),
        (
            "InspectorSearch editing",
            |s, _| {
                presentation::reducer::open(s, Surface::Help);
                s.help_overlay.search.editing = true;
            },
            false,
        ),
        (
            "the command palette (a text field)",
            |s, _| {
                presentation::reducer::open(s, Surface::CommandPalette);
            },
            false,
        ),
        (
            "the quit guard (L4)",
            |s, _| {
                let mut quit = QuitFacts::idle(QuitHost::Local);
                quit.composer_draft = true;
                presentation::reducer::request_quit(s, quit);
            },
            false,
        ),
        (
            "a pending leader",
            |s, _| {
                handle_input(s, &special(DomainKey::CtrlX));
            },
            false,
        ),
    ]
}

#[test]
fn k2_each_protected_state_queues_the_arrival_and_its_positive_control_takes_the_frame() {
    for (case, setup, takes_frame) in cases() {
        let (mut state, clock) = clocked(80, 24);
        state.focus = FocusState::Input;
        setup(&mut state, &clock);
        let stack_before = state.modal_stack.len();
        handle_upsert_context_warning(&mut state, "c1", 85);
        assert_eq!(waiting(&state), 1, "{case}: the item is held either way");
        assert!(presentation::consistent(&state), "{case}");
        if takes_frame {
            assert!(
                is_open(&state, Surface::Decision),
                "{case}: takes the frame"
            );
            assert_eq!(toasts(&state), 0, "{case}: no toast when it takes focus");
        } else {
            assert!(!is_open(&state, Surface::Decision), "{case}: must queue");
            assert_eq!(
                state.modal_stack.len(),
                stack_before,
                "{case}: stack untouched"
            );
            assert_eq!(toasts(&state), 1, "{case}: exactly one waiting toast");
        }
    }
}

#[test]
fn only_a_zero_to_one_arrival_may_preempt() {
    let (mut state, clock) = clocked(80, 24);
    state.focus = FocusState::Input;
    state.input_buffer = "draft".into();
    // A waits behind the typing user: the queue is now non-empty.
    handle_upsert_context_warning(&mut state, "c1", 85);
    assert!(!is_open(&state, Surface::Decision));
    // The user finishes and the typing grace passes: the composer is idle and empty.
    state.input_buffer.clear();
    state.cursor_position = 0;
    advance(&clock, Duration::from_secs(2));
    // B is a 1→2 arrival: it waits, however idle the user now is.
    handle_upsert_context_warning(&mut state, "c2", 90);
    assert!(!is_open(&state, Surface::Decision), "1→2 never preempts");
    assert_eq!(waiting(&state), 2);
    assert_eq!(
        toasts(&state),
        1,
        "identical waiting toasts coalesce into one"
    );
    // Positive control: with the queue empty again, the same idle arrival takes the frame.
    handle_clear_context_warning(&mut state, "c1");
    handle_clear_context_warning(&mut state, "c2");
    assert!(state.decisions.is_empty());
    handle_upsert_context_warning(&mut state, "c3", 90);
    assert!(
        is_open(&state, Surface::Decision),
        "0→1 takes the idle frame"
    );
}

#[test]
fn k2_a_deferral_blocks_automatic_arrivals_for_exactly_sixty_seconds() {
    for (after, takes_frame) in [(59u64, false), (61, true)] {
        let (mut state, clock) = clocked(80, 24);
        let f = facts("c1");
        handle_upsert_context_warning(&mut state, "c1", 85);
        ready(&mut state, &clock);
        route(&mut state, &special(DomainKey::Esc), &f);
        assert!(!is_open(&state, Surface::Decision), "Esc deferred it");
        handle_clear_context_warning(&mut state, "c1");
        assert!(state.decisions.is_empty());
        advance(&clock, Duration::from_secs(after));
        handle_upsert_context_warning(&mut state, "c1", 90);
        assert_eq!(is_open(&state, Surface::Decision), takes_frame, "{after} s");
    }
}

#[test]
fn k2_typed_letters_never_become_decisions() {
    let (mut state, _clock) = clocked(80, 24);
    state.focus = FocusState::Input;
    state.input_buffer = "type ".into();
    state.cursor_position = 5;
    handle_upsert_context_warning(&mut state, "c1", 85);
    for c in ['y', 'n', 'k'] {
        handle_input(&mut state, &key(c));
    }
    assert_eq!(
        state.input_buffer, "type ynk",
        "the letters went to the composer"
    );
    assert!(
        state.decisions.holds(DecisionKind::ContextWarning),
        "still waiting"
    );
    assert!(!is_open(&state, Surface::Decision));
}

#[test]
fn k2_a_requested_decision_opens_at_once_except_over_the_quit_guard_or_a_text_field() {
    let delete = |state: &mut TuiState| begin_delete_confirmation(state, delete_target());
    // Over Help (a reader): opens at once, even though a deferral is recent.
    let (mut state, _clock) = clocked(80, 24);
    presentation::reducer::open(&mut state, Surface::Help);
    delete(&mut state);
    assert_eq!(
        state.modal_stack.top().map(|frame| frame.surface),
        Some(Surface::Decision)
    );
    // Over S-QUIT (L4): queues behind it.
    let (mut state, _clock) = clocked(80, 24);
    let mut quit = QuitFacts::idle(QuitHost::Local);
    quit.composer_draft = true;
    presentation::reducer::request_quit(&mut state, quit);
    delete(&mut state);
    assert_eq!(
        state.modal_stack.top().map(|frame| frame.surface),
        Some(Surface::QuitConfirm)
    );
    assert_eq!(waiting(&state), 1);
    assert_eq!(toasts(&state), 1);
    // Over a text field: queues behind it.
    let (mut state, _clock) = clocked(80, 24);
    presentation::reducer::open(&mut state, Surface::CommandPalette);
    delete(&mut state);
    assert!(!is_open(&state, Surface::Decision));
    assert_eq!(waiting(&state), 1);
}

#[test]
fn k2_a_producer_for_another_conversation_waits_and_is_never_answered_in_the_active_one() {
    let (mut state, clock) = clocked(80, 24);
    let f = facts("a");
    // The draw has told the queue which conversation the user is in.
    decision_queue::prune_closed(&mut state, &f);
    handle_upsert_context_warning(&mut state, "b", 85);
    assert!(
        !is_open(&state, Surface::Decision),
        "b's warning must not take a's screen"
    );
    assert_eq!(waiting(&state), 1);
    assert_eq!(toasts(&state), 1);
    let item = state.decisions.items().next().unwrap();
    assert_eq!(item.conversation_id.as_deref(), Some("b"));
    // The list includes b, but Enter in a cannot open it.
    decision_queue::open_waiting(&mut state, &f);
    assert!(is_open(&state, Surface::DecisionQueue));
    route(&mut state, &special(DomainKey::Enter), &f);
    assert!(!is_open(&state, Surface::Decision));
    assert!(is_open(&state, Surface::DecisionQueue));
    ready(&mut state, &clock);
    // Back in b, opening the selected row focuses the same item.
    route(&mut state, &special(DomainKey::Enter), &facts("b"));
    assert!(is_open(&state, Surface::Decision));
    // Positive control: the same producer for the active conversation takes the frame.
    let (mut state, _clock) = clocked(80, 24);
    decision_queue::prune_closed(&mut state, &f);
    handle_upsert_context_warning(&mut state, "a", 85);
    assert!(is_open(&state, Surface::Decision));
}
