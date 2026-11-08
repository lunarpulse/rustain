//! K7 (slice 6-C): AskUserQuestion is a queue item with a text field of its own. The
//! producer is `decision_queue::raise_question` (the fn the event-loop arm calls);
//! keys go through `presentation::route`; the answer reaches the waiting turn through
//! the carried oneshot only after the pane resolved the item, exactly once.

use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rustain::adapters::tui::app::{
    InputAction, convert_crossterm_event, handle_input, handle_operator_input,
};
use rustain::adapters::tui::decision_queue::{
    self, DecisionEffect, DecisionKind, DecisionSubject, QuestionPayload, Raised, Validity,
};
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface, reducer};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::clock::MockClock;
use rustain::domain::events::DomainKey;
use rustain::domain::models::{FocusState, MouseConfig};
use tokio::sync::oneshot::{self, error::TryRecvError};

use super::raisers::{Hold, raise};
use super::support::{clocked, facts, key, paint, ready, route, screen, special};

fn payload(state: &TuiState) -> &QuestionPayload {
    state
        .decisions
        .items()
        .find_map(|item| match &item.subject {
            DecisionSubject::Question(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .expect("a question is queued")
}

/// A painted, ready question over a full composer in Chat focus (typing in the
/// composer is not the user's current act, so the arrival may take the frame).
fn open(width: u16, height: u16) -> (TuiState, Arc<MockClock>, oneshot::Receiver<String>) {
    let (mut state, clock) = clocked(width, height);
    state.input_buffer = "my unsent draft".into();
    state.cursor_position = 7;
    state.focus = FocusState::Chat;
    let (raised, hold) = raise(&mut state, DecisionKind::Question, "Which colour?");
    assert_eq!(raised, Raised::Focused, "an idle Stage takes the question");
    let Hold::Question(rx) = hold else {
        panic!("question holds a receiver")
    };
    paint(&mut state, &facts("c1"), width, height);
    ready(&mut state, &clock);
    (state, clock, rx)
}

fn type_text(state: &mut TuiState, text: &str) {
    for c in text.chars() {
        assert_eq!(
            route(state, &key(c), &facts("c1")),
            Some(RouteEffect::Consumed),
            "{c:?} types into the field"
        );
    }
}

/// What the event loop's `InputAction::Decision` arm does after the pane resolved.
fn deliver(state: &mut TuiState, effect: Option<RouteEffect>) -> DecisionEffect {
    let Some(RouteEffect::Action(action @ InputAction::Decision(effect))) = effect else {
        panic!("a question answer is a decision effect, got {effect:?}")
    };
    assert!(!action.is_mutation(), "a read-only tab still delivers it");
    decision_queue::apply_resolved_action(state, action, reducer::restore_focus_after_modal);
    effect
}

fn chord(state: &mut TuiState, k: DomainKey) -> Option<RouteEffect> {
    route(state, &special(k), &facts("c1"))
}

fn ctrl_x_q(state: &mut TuiState) {
    let _ = handle_input(state, &special(DomainKey::CtrlX));
    assert_eq!(handle_input(state, &key('q')), InputAction::FocusDecisions);
    decision_queue::open_waiting(state, &facts("c1"));
}

#[test]
fn answer_typing_never_touches_the_composer() {
    let (mut state, _clock, _rx) = open(80, 24);
    let composer = (state.input_buffer.clone(), state.cursor_position);
    type_text(&mut state, "blue");
    assert_eq!(payload(&state).field.as_str(), "blue");
    assert_eq!(
        (state.input_buffer.clone(), state.cursor_position),
        composer,
        "typing went to the frame field"
    );
    let text = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(text.contains("blue"), "{text}");
    assert!(
        text.contains("[Enter] Send  [Ctrl+D] Skip question  [Esc] Back"),
        "{text}"
    );
    // Readline works inside the field: Ctrl+A / Ctrl+E move its caret, Ctrl+K kills to the end.
    assert_eq!(
        chord(&mut state, DomainKey::CtrlA),
        Some(RouteEffect::Consumed)
    );
    assert_eq!(payload(&state).field.cursor(), 0);
    type_text(&mut state, "x");
    assert_eq!(payload(&state).field.as_str(), "xblue");
    assert_eq!(
        chord(&mut state, DomainKey::CtrlE),
        Some(RouteEffect::Consumed)
    );
    assert_eq!(payload(&state).field.cursor(), 5);
    assert_eq!(
        chord(&mut state, DomainKey::CtrlA),
        Some(RouteEffect::Consumed)
    );
    type_text(&mut state, "!");
    assert_eq!(
        chord(&mut state, DomainKey::CtrlK),
        Some(RouteEffect::Consumed)
    );
    assert_eq!(
        payload(&state).field.as_str(),
        "!",
        "Ctrl+K killed to the line end"
    );
    assert_eq!(
        (state.input_buffer.clone(), state.cursor_position),
        composer,
        "readline keys never reach the composer either"
    );
    // Positive control: with no frame, the same keys DO reach the composer.
    let mut plain = TuiState::new(80, 24);
    let _ = handle_input(&mut plain, &key('z'));
    assert_eq!(plain.input_buffer, "z");
}

#[tokio::test]
async fn enter_sends_the_answer_once_and_the_item_leaves() {
    let (mut state, _clock, mut rx) = open(80, 24);
    type_text(&mut state, "blue");
    assert_eq!(
        rx.try_recv(),
        Err(TryRecvError::Empty),
        "typing sends nothing"
    );
    let effect = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    assert_eq!(
        deliver(&mut state, effect),
        DecisionEffect::QuestionSend,
        "Enter on the field sends"
    );
    assert_eq!(rx.await.expect("the turn got the answer"), "blue");
    assert_eq!(decision_queue::waiting(&state), 0);
    assert!(!presentation::is_open(&state, Surface::Decision));
    // A second Enter finds nothing to answer.
    let again = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    assert!(
        !matches!(again, Some(RouteEffect::Action(InputAction::Decision(_)))),
        "{again:?}"
    );
}

#[tokio::test]
async fn esc_defers_and_the_turn_keeps_waiting() {
    let (mut state, clock, mut rx) = open(80, 24);
    type_text(&mut state, "blue");
    // First Esc leaves the field, the frame stays and nothing was decided.
    assert_eq!(
        route(&mut state, &special(DomainKey::Esc), &facts("c1")),
        Some(RouteEffect::Consumed)
    );
    assert!(presentation::is_open(&state, Surface::Decision));
    let text = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(
        text.contains("[Enter] Answer  [Ctrl+D] Skip question  [Esc] Later"),
        "{text}"
    );
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    // Second Esc defers: the frame closes, the item stays queued and counted,
    // the oneshot is alive and the turn keeps waiting.
    assert_eq!(
        route(&mut state, &special(DomainKey::Esc), &facts("c1")),
        Some(RouteEffect::Consumed)
    );
    assert!(!presentation::is_open(&state, Surface::Decision));
    assert_eq!(decision_queue::waiting(&state), 1);
    assert!(state.decisions.items().all(|item| item.deferred));
    assert!(!payload(&state).reply.is_closed(), "the reply is alive");
    assert_eq!(
        rx.try_recv(),
        Err(TryRecvError::Empty),
        "the turn still waits"
    );
    // Ctrl+X q reopens it with the typed text intact; Enter answers once.
    ctrl_x_q(&mut state);
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    assert!(presentation::is_open(&state, Surface::Decision));
    paint(&mut state, &facts("c1"), 80, 24);
    super::support::advance(&clock, decision_queue::INERT_AFTER_PAINT);
    assert_eq!(payload(&state).field.as_str(), "blue");
    let effect = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    assert_eq!(deliver(&mut state, effect), DecisionEffect::QuestionSend);
    assert_eq!(rx.await.expect("answered"), "blue");
    assert_eq!(decision_queue::waiting(&state), 0);
}

#[tokio::test]
async fn ctrl_d_skips_by_dropping_the_sender_and_ctrl_c_never_does() {
    let (mut state, clock, mut rx) = open(80, 24);
    type_text(&mut state, "half");
    // Ctrl+C is back one frame (leaves the field, then defers): never a skip.
    for _ in 0..2 {
        let _ = route(&mut state, &special(DomainKey::CtrlC), &facts("c1"));
    }
    assert_eq!(
        rx.try_recv(),
        Err(TryRecvError::Empty),
        "Ctrl+C is not a skip"
    );
    assert_eq!(decision_queue::waiting(&state), 1);
    ctrl_x_q(&mut state);
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    paint(&mut state, &facts("c1"), 80, 24);
    super::support::advance(&clock, decision_queue::INERT_AFTER_PAINT);
    let effect = route(&mut state, &special(DomainKey::CtrlD), &facts("c1"));
    assert_eq!(deliver(&mut state, effect), DecisionEffect::QuestionSkip);
    assert!(rx.await.is_err(), "the sender was dropped: RecvError");
    assert_eq!(decision_queue::waiting(&state), 0);
}

#[tokio::test]
async fn a_question_raised_for_tab_b_is_answered_into_its_own_turn_from_tab_a() {
    let (mut state, clock) = clocked(80, 24);
    let (tx_b, mut rx_b) = oneshot::channel();
    let (tx_a, mut rx_a) = oneshot::channel();
    decision_queue::raise_question(
        &mut state,
        "b",
        "tu-b".into(),
        "Question for B?".into(),
        tx_b,
    );
    decision_queue::raise_question(
        &mut state,
        "a",
        "tu-a".into(),
        "Question for A?".into(),
        tx_a,
    );
    assert_eq!(decision_queue::waiting(&state), 2);
    // Tab A is the active one; the frame shows the oldest decidable item (B's).
    paint(&mut state, &facts("a"), 80, 24);
    ready(&mut state, &clock);
    let text = screen(&paint(&mut state, &facts("a"), 80, 24));
    assert!(text.contains("Question for B?"), "{text}");
    for c in "to-b".chars() {
        let _ = route(&mut state, &key(c), &facts("a"));
    }
    let effect = route(&mut state, &special(DomainKey::Enter), &facts("a"));
    deliver(&mut state, effect);
    assert_eq!(
        rx_b.try_recv(),
        Ok("to-b".to_string()),
        "landed in B's own turn"
    );
    assert_eq!(
        rx_a.try_recv(),
        Err(TryRecvError::Empty),
        "A's turn is untouched"
    );
    assert_eq!(decision_queue::waiting(&state), 1);
    // Control: A's own question is still queued and answerable by its own key.
    assert!(
        state
            .decisions
            .items()
            .all(|item| payload_origin(item) == "a")
    );
}

fn payload_origin(item: &decision_queue::DecisionItem) -> String {
    match &item.subject {
        DecisionSubject::Question(payload) => payload.origin_conversation.clone(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_closed_turn_turns_the_question_stale_and_a_live_one_is_the_control() {
    let (mut state, _clock, rx) = open(80, 24);
    drop(rx);
    decision_queue::prune_closed(&mut state, &facts("c1"));
    let item = state.decisions.items().next().expect("still listed");
    assert!(
        matches!(item.validity, Validity::Stale(_)),
        "a receiver that went away makes the question stale"
    );
    let text = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(
        text.contains("This decision is no longer pending."),
        "{text}"
    );
    assert!(text.contains("[Esc] Close"), "{text}");
    assert_ne!(
        route(&mut state, &special(DomainKey::Enter), &facts("c1")),
        Some(RouteEffect::Action(InputAction::Decision(
            DecisionEffect::QuestionSend
        )))
    );
    // Control: a live receiver keeps the question live through the same prune.
    let (mut live, _clock2, _rx2) = open(80, 24);
    decision_queue::prune_closed(&mut live, &facts("c1"));
    assert!(matches!(
        live.decisions.items().next().unwrap().validity,
        Validity::Live
    ));
}

#[tokio::test]
async fn cancelling_the_turn_abandons_every_reply_and_empties_the_queue() {
    let (mut state, _clock) = clocked(80, 24);
    let (tx1, rx1) = oneshot::channel::<String>();
    let (tx2, rx2) = oneshot::channel::<String>();
    decision_queue::raise_question(&mut state, "c1", "t1".into(), "one?".into(), tx1);
    decision_queue::raise_question(&mut state, "c1", "t2".into(), "two?".into(), tx2);
    assert_eq!(decision_queue::waiting(&state), 2);
    decision_queue::drain_questions(&mut state, "c1");
    assert!(
        rx1.await.is_err() && rx2.await.is_err(),
        "both turns see RecvError"
    );
    // The one whose frame is open turns stale (its Close removes it), nothing is decidable.
    assert_eq!(state.decisions.undecided(), 0);
    assert!(state.decisions.items().all(|item| item.is_stale()));
    // Control: draining an empty queue is a no-op.
    let (mut empty, _clock) = clocked(80, 24);
    decision_queue::drain_questions(&mut empty, "c1");
    assert!(empty.decisions.is_empty());
}

fn raw(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Event {
    Event::Key(KeyEvent::new_with_kind(code, modifiers, kind))
}

fn raw_input(state: &mut TuiState, event: &Event) -> Option<InputAction> {
    let domain = convert_crossterm_event(event, &MouseConfig::default())?;
    handle_operator_input(state, event, &domain)
}

#[test]
fn only_a_plain_press_sends_and_shift_text_still_types() {
    let (mut state, _clock, _rx) = open(80, 24);
    // SHIFT text types (a capital is not a command).
    let shifted = raw(KeyCode::Char('B'), KeyModifiers::SHIFT, KeyEventKind::Press);
    let _ = raw_input(&mut state, &shifted);
    assert_eq!(payload(&state).field.as_str(), "B");
    // A release or a repeat of Enter never sends.
    for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
        let sent = raw_input(&mut state, &raw(KeyCode::Enter, KeyModifiers::NONE, kind));
        assert_ne!(
            sent,
            Some(InputAction::Decision(DecisionEffect::QuestionSend)),
            "{kind:?}"
        );
        assert_eq!(decision_queue::waiting(&state), 1, "{kind:?}");
    }
    // Control: the plain press sends.
    let sent = raw_input(
        &mut state,
        &raw(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press),
    );
    assert_eq!(
        sent,
        Some(InputAction::Decision(DecisionEffect::QuestionSend))
    );
}

/// The field is left (one Esc): the rail now says `[Enter] Answer`.
fn leave_field(state: &mut TuiState) {
    assert_eq!(
        route(state, &special(DomainKey::Esc), &facts("c1")),
        Some(RouteEffect::Consumed)
    );
    assert!(screen(&paint(state, &facts("c1"), 80, 24)).contains("[Enter] Answer"));
}

#[tokio::test]
async fn ctrl_d_after_leaving_the_answer_field_skips_once_and_closes_the_receiver() {
    let (mut state, _clock, mut rx) = open(80, 24);
    type_text(&mut state, "half");
    leave_field(&mut state);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "Esc is not a skip");
    let effect = route(&mut state, &special(DomainKey::CtrlD), &facts("c1"));
    assert_eq!(deliver(&mut state, effect), DecisionEffect::QuestionSkip);
    assert_eq!(
        rx.try_recv(),
        Err(TryRecvError::Closed),
        "the turn got RecvError"
    );
    assert_eq!(decision_queue::waiting(&state), 0);
    // Exactly once: a second Ctrl+D has nothing to skip.
    let again = route(&mut state, &special(DomainKey::CtrlD), &facts("c1"));
    assert!(
        !matches!(again, Some(RouteEffect::Action(InputAction::Decision(_)))),
        "{again:?}"
    );
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
}

/// Raw Enter/Ctrl+D in the open field and after leaving it: only an unmodified (Enter)
/// or Ctrl-only (Ctrl+D) Press decides; everything else is dropped by the raw guard.
#[tokio::test]
async fn question_and_idle_question_deciding_keys_need_a_plain_press() {
    use KeyEventKind::{Press, Release, Repeat};
    for idle in [false, true] {
        let enter_ignored = [
            (KeyModifiers::NONE, Repeat),
            (KeyModifiers::NONE, Release),
            (KeyModifiers::SHIFT, Press),
            (KeyModifiers::ALT, Press),
            (KeyModifiers::CONTROL, Press),
        ];
        for (modifiers, kind) in enter_ignored {
            let (mut state, _clock, mut rx) = open(80, 24);
            type_text(&mut state, "ab");
            if idle {
                leave_field(&mut state);
            }
            let at = format!("idle={idle} Enter {modifiers:?} {kind:?}");
            assert_eq!(
                raw_input(&mut state, &raw(KeyCode::Enter, modifiers, kind)),
                None,
                "{at}"
            );
            assert_eq!(decision_queue::waiting(&state), 1, "{at}");
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{at}");
            assert_eq!(payload(&state).field.as_str(), "ab", "{at}");
        }
        for kind in [Repeat, Release] {
            let (mut state, _clock, mut rx) = open(80, 24);
            if idle {
                leave_field(&mut state);
            }
            let at = format!("idle={idle} Ctrl+D {kind:?}");
            let event = raw(KeyCode::Char('d'), KeyModifiers::CONTROL, kind);
            assert_eq!(raw_input(&mut state, &event), None, "{at}");
            assert_eq!(decision_queue::waiting(&state), 1, "{at}");
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{at}");
        }
        // Positive controls: the plain Press decides (Send in the field, Answer reopens it).
        let (mut state, _clock, mut rx) = open(80, 24);
        type_text(&mut state, "ab");
        if idle {
            leave_field(&mut state);
        }
        let enter = raw(KeyCode::Enter, KeyModifiers::NONE, Press);
        let action = raw_input(&mut state, &enter).expect("a plain Enter Press is handled");
        if idle {
            assert_eq!(action, InputAction::Consumed, "Answer reopens the field");
            assert!(screen(&paint(&mut state, &facts("c1"), 80, 24)).contains("[Enter] Send"));
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        } else {
            assert_eq!(action, InputAction::Decision(DecisionEffect::QuestionSend));
            decision_queue::apply_resolved_action(
                &mut state,
                action,
                reducer::restore_focus_after_modal,
            );
            assert_eq!(rx.try_recv(), Ok("ab".to_owned()));
        }
        let (mut state, _clock, mut rx) = open(80, 24);
        if idle {
            leave_field(&mut state);
        }
        let ctrl_d = raw(KeyCode::Char('d'), KeyModifiers::CONTROL, Press);
        let action = raw_input(&mut state, &ctrl_d).expect("a Ctrl+D Press is handled");
        assert_eq!(action, InputAction::Decision(DecisionEffect::QuestionSkip));
        decision_queue::apply_resolved_action(
            &mut state,
            action,
            reducer::restore_focus_after_modal,
        );
        assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    }
}

#[tokio::test]
async fn draining_one_conversation_leaves_another_conversations_question_waiting() {
    let (mut state, _clock) = clocked(80, 24);
    let (tx_a, rx_a) = oneshot::channel::<String>();
    let (tx_b, mut rx_b) = oneshot::channel::<String>();
    // The same provider tool-use id in two conversations is two different questions.
    decision_queue::raise_question(&mut state, "a", "tu".into(), "for a?".into(), tx_a);
    decision_queue::raise_question(&mut state, "b", "tu".into(), "for b?".into(), tx_b);
    assert_eq!(state.decisions.len(), 2, "no identity collision");
    decision_queue::drain_questions(&mut state, "a");
    assert!(rx_a.await.is_err(), "a's turn sees RecvError");
    assert_eq!(rx_b.try_recv(), Err(TryRecvError::Empty), "b keeps waiting");
    assert_eq!(
        state
            .decisions
            .items()
            .filter(|item| !item.is_stale())
            .map(payload_origin)
            .collect::<Vec<_>>(),
        ["b"]
    );
}
