//! K13: ticket queue admission, answer validation and missed-call presentation.
use super::support::{clocked, facts, key, paint, ready, route, screen, special};
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{self, DecisionEffect, Responder, TicketPayload};
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface};
use rustain::domain::events::DomainKey;
use rustain::domain::models::{AgentId, ArtifactId, ContentHash};
use serde_json::json;

fn ticket(schema: serde_json::Value) -> TicketPayload {
    let node = AgentId::parse("mcp-task").unwrap();
    let artifact = ArtifactId::from(ContentHash::parse_hex(&"a".repeat(64)).unwrap());
    TicketPayload::new(
        node.clone(),
        artifact,
        node,
        7,
        0,
        json!({"key":"request", "request":{"method":"elicitation/create", "params":{
            "message":"Proceed?", "requestedSchema":schema}}}),
    )
    .unwrap()
}

#[test]
fn story21_6_ticket_accumulates_and_invalid_answer_stays_open() {
    let (mut state, clock) = clocked(80, 24);
    presentation::reducer::open(&mut state, Surface::Help);
    decision_queue::raise_ticket(
        &mut state,
        ticket(json!({"type":"object", "required":["n"],
        "properties":{"n":{"type":"integer", "minimum":1}}})),
        Responder::Local,
    );
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
    assert_eq!(decision_queue::waiting(&state), 1);
    decision_queue::open_waiting(&mut state, &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    ready(&mut state, &clock);
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    let painted = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(painted.contains("Error:"), "{painted}");
    assert!(painted.contains("time unknown"), "{painted}");
    assert!(
        painted.contains("not an authenticated instruction"),
        "{painted}"
    );
    assert_eq!(decision_queue::waiting(&state), 1);
}

#[test]
fn story21_6_expired_ticket_remains_visible_until_closed() {
    let (mut state, _) = clocked(80, 24);
    let payload = ticket(json!({"type":"object","properties":{}}));
    let artifact = payload.artifact.clone();
    let node = payload.node.clone();
    decision_queue::raise_ticket(&mut state, payload, Responder::Attached);
    decision_queue::resolve_ticket(&mut state, &node, &artifact, true);
    assert_eq!(decision_queue::waiting(&state), 1);
    decision_queue::open_waiting(&mut state, &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    let painted = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(
        painted.contains("Missed: the tool stopped waiting"),
        "{painted}"
    );
    let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    assert_eq!(decision_queue::waiting(&state), 0);
}

#[test]
fn story21_6_ticket_rails_and_effects_are_distinct() {
    for (input, expected) in [
        ('d', DecisionEffect::TicketDecline),
        ('x', DecisionEffect::TicketCancel),
    ] {
        let (mut state, clock) = clocked(80, 24);
        decision_queue::raise_ticket(
            &mut state,
            ticket(json!({"type":"boolean"})),
            Responder::Local,
        );
        decision_queue::open_waiting(&mut state, &facts("c1"));
        let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
        let painted = screen(&paint(&mut state, &facts("c1"), 80, 24));
        assert!(painted.contains("Decline"), "{painted}");
        assert!(painted.contains("Cancel task"), "{painted}");
        assert!(
            painted.contains("Decline continues the task; Cancel ends it."),
            "{painted}"
        );
        ready(&mut state, &clock);
        assert_eq!(
            route(&mut state, &key(input), &facts("c1")),
            Some(RouteEffect::Action(InputAction::Decision(expected)))
        );
        assert_eq!(
            decision_queue::waiting(&state),
            1,
            "send settlement owns retirement"
        );
    }
}

#[test]
fn story21_6_ticket_deciding_keys_require_plain_press() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use rustain::adapters::tui::app;
    use rustain::domain::events::DomainInputEvent;
    for code in [KeyCode::Enter, KeyCode::Char('d'), KeyCode::Char('x')] {
        for (modifiers, kind) in [
            (KeyModifiers::NONE, KeyEventKind::Repeat),
            (KeyModifiers::NONE, KeyEventKind::Release),
            (KeyModifiers::SHIFT, KeyEventKind::Press),
            (KeyModifiers::ALT, KeyEventKind::Press),
            (KeyModifiers::CONTROL, KeyEventKind::Press),
        ] {
            let (mut state, _) = clocked(80, 24);
            decision_queue::raise_ticket(
                &mut state,
                ticket(json!({"type":"boolean"})),
                Responder::Local,
            );
            decision_queue::open_waiting(&mut state, &facts("c1"));
            let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
            let raw = Event::Key(KeyEvent::new_with_kind(code, modifiers, kind));
            let passed =
                app::handle_operator_input(&mut state, &raw, &DomainInputEvent::FocusGained);
            if code == KeyCode::Char('x')
                && modifiers == KeyModifiers::CONTROL
                && kind == KeyEventKind::Press
            {
                assert!(passed.is_some(), "Ctrl+X is the non-deciding leader");
            } else {
                assert!(passed.is_none());
            }
            assert_eq!(decision_queue::waiting(&state), 1);
        }
        let (mut state, _) = clocked(80, 24);
        decision_queue::raise_ticket(
            &mut state,
            ticket(json!({"type":"boolean"})),
            Responder::Local,
        );
        decision_queue::open_waiting(&mut state, &facts("c1"));
        let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
        let raw = Event::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert!(
            app::handle_operator_input(&mut state, &raw, &DomainInputEvent::FocusGained).is_some()
        );
    }
}

#[test]
fn story21_6_local_ticket_and_reply_raw_ctrl_x_opens_leader_and_queue() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use rustain::adapters::tui::{app, keymap::Host};
    for reply in [false, true] {
        let (mut state, _) = clocked(80, 24);
        if reply {
            decision_queue::reply::observe(&mut state, "principal", "alias", "ri_1", 1);
        } else {
            decision_queue::raise_ticket(
                &mut state,
                ticket(json!({"type":"boolean"})),
                Responder::Local,
            );
        }
        decision_queue::open_waiting(&mut state, &facts("c1"));
        let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
        for (code, modifiers) in [
            (KeyCode::Char('x'), KeyModifiers::CONTROL),
            (KeyCode::Char('q'), KeyModifiers::NONE),
        ] {
            let raw = Event::Key(KeyEvent::new(code, modifiers));
            assert!(!app::frame_ignores_raw_key(&state, &raw));
            let input = app::convert_crossterm_event(&raw, &Default::default()).unwrap();
            let result = presentation::route(&mut state, &input, Host::Local, &facts("c1"));
            if modifiers == KeyModifiers::CONTROL {
                assert!(state.pending_leader.is_some());
            } else {
                assert_eq!(
                    result,
                    Some(RouteEffect::Action(InputAction::FocusDecisions))
                );
                decision_queue::open_waiting(&mut state, &facts("c1"));
                assert!(presentation::is_open(&state, Surface::DecisionQueue));
            }
        }
    }
}

#[test]
fn story21_6_ticket_text_field_enter_guard_preserves_typed_text() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use rustain::adapters::tui::app;
    use rustain::domain::events::DomainInputEvent;
    for (modifiers, kind) in [
        (KeyModifiers::NONE, KeyEventKind::Repeat),
        (KeyModifiers::NONE, KeyEventKind::Release),
        (KeyModifiers::SHIFT, KeyEventKind::Press),
        (KeyModifiers::ALT, KeyEventKind::Press),
        (KeyModifiers::CONTROL, KeyEventKind::Press),
    ] {
        let (mut state, clock) = clocked(80, 24);
        decision_queue::raise_ticket(
            &mut state,
            ticket(json!({"type":"string"})),
            Responder::Local,
        );
        decision_queue::open_waiting(&mut state, &facts("c1"));
        let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
        ready(&mut state, &clock);
        let _ = route(&mut state, &key('e'), &facts("c1"));
        let _ = route(&mut state, &key('z'), &facts("c1"));
        let raw = Event::Key(KeyEvent::new_with_kind(KeyCode::Enter, modifiers, kind));
        assert!(
            app::handle_operator_input(&mut state, &raw, &DomainInputEvent::FocusGained).is_none()
        );
        let item = state.decisions.items().next().unwrap();
        let decision_queue::DecisionSubject::Ticket(payload) = &item.subject else {
            panic!("ticket")
        };
        assert_eq!(payload.editing_field().unwrap().as_str(), "z");
        assert!(state.input_buffer.is_empty());
    }
}

#[test]
fn story21_6_ticket_replay_orders_by_sequence_and_resolution_is_node_scoped() {
    let (mut state, _) = clocked(80, 24);
    let later = ticket(json!({"type":"boolean"}));
    let mut earlier = later.clone();
    earlier.node = AgentId::parse("other-node").unwrap();
    earlier.seq = 2;
    let node = later.node.clone();
    let artifact = later.artifact.clone();
    decision_queue::raise_ticket(&mut state, later, Responder::Local);
    decision_queue::raise_ticket(&mut state, earlier, Responder::Local);
    let sequences: Vec<_> = state
        .decisions
        .items()
        .map(|item| match &item.subject {
            decision_queue::DecisionSubject::Ticket(ticket) => ticket.seq,
            _ => panic!("ticket"),
        })
        .collect();
    assert_eq!(sequences, [2, 7]);
    decision_queue::resolve_ticket(&mut state, &node, &artifact, true);
    let stale: Vec<_> = state
        .decisions
        .items()
        .map(|item| item.is_stale())
        .collect();
    assert_eq!(stale, [false, true]);
}

fn answer_of(state: &rustain::adapters::tui::state::TuiState) -> serde_json::Value {
    match &state.decisions.items().next().expect("a ticket").subject {
        decision_queue::DecisionSubject::Ticket(ticket) => ticket.answer().expect("a valid form"),
        other => panic!("not a ticket: {other:?}"),
    }
}

/// The registry rows `Up`/`Down` (previous/next field), `Left`/`Right` (previous/next
/// answer) and `e` (edit) each change the form (the witness of every Ticket row).
#[test]
fn story21_6_ticket_selection_and_edit_keys_change_the_form() {
    let (mut state, clock) = clocked(80, 24);
    decision_queue::raise_ticket(
        &mut state,
        ticket(json!({"type":"object", "properties":{
            "a":{"type":"boolean"}, "b":{"type":"boolean"}, "t":{"type":"string"}}})),
        Responder::Local,
    );
    decision_queue::open_waiting(&mut state, &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    ready(&mut state, &clock);
    let first = answer_of(&state);
    let _ = route(&mut state, &special(DomainKey::Right), &facts("c1"));
    let toggled_a = answer_of(&state);
    assert_ne!(toggled_a, first, "Right changes the focused field's answer");
    let _ = route(&mut state, &special(DomainKey::Left), &facts("c1"));
    assert_eq!(
        answer_of(&state),
        first,
        "Left returns to the previous answer"
    );
    let _ = route(&mut state, &special(DomainKey::Down), &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Right), &facts("c1"));
    let toggled_b = answer_of(&state);
    assert_ne!(toggled_b, first, "Down moves to the next field");
    assert_eq!(
        toggled_b["a"], first["a"],
        "field a is untouched after Down"
    );
    let _ = route(&mut state, &special(DomainKey::Up), &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Right), &facts("c1"));
    assert_ne!(answer_of(&state)["a"], first["a"], "Up returns to field a");
    let _ = route(&mut state, &special(DomainKey::Down), &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Down), &facts("c1"));
    let _ = route(&mut state, &key('e'), &facts("c1"));
    match &state.decisions.items().next().unwrap().subject {
        decision_queue::DecisionSubject::Ticket(ticket) => {
            assert!(ticket.editing_field().is_some(), "`e` opens the text field");
        }
        other => panic!("not a ticket: {other:?}"),
    }
}

#[test]
fn story21_6_ticket_floor_keeps_body_and_rail_idle_and_editing() {
    let (mut state, clock) = clocked(60, 16);
    state.input_buffer = "full composer\n".repeat(20);
    decision_queue::raise_ticket(
        &mut state,
        ticket(json!({"type":"string"})),
        Responder::Local,
    );
    decision_queue::open_waiting(&mut state, &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    let idle = screen(&paint(&mut state, &facts("c1"), 60, 16));
    assert!(idle.contains("Proceed?"), "{idle}");
    assert!(idle.contains("Accept"), "{idle}");
    assert!(idle.contains("Cancel task"), "{idle}");
    ready(&mut state, &clock);
    let _ = route(&mut state, &key('e'), &facts("c1"));
    let editing = screen(&paint(&mut state, &facts("c1"), 60, 16));
    assert!(editing.contains("Proceed?"), "{editing}");
    assert!(editing.contains("Answer:"), "{editing}");
    assert!(editing.contains("Accept"), "{editing}");
    state.terminal_height = 4;
    assert_eq!(
        decision_queue::handle_key(&mut state, decision_queue::PaneKey::Enter, &facts("c1")),
        decision_queue::PaneOutcome::Consumed,
    );
    assert_eq!(decision_queue::waiting(&state), 1);
}

#[test]
fn story21_6_ticket_reoffer_resets_reviewed_identity_and_inert_window() {
    for responder in [Responder::Local, Responder::Attached] {
        for changed in [false, true] {
            let (mut state, clock) = clocked(80, 24);
            let old = ticket(json!({"type":"boolean"}));
            let (node, artifact) = (old.node.clone(), old.artifact.clone());
            decision_queue::raise_ticket(&mut state, old.clone(), responder);
            decision_queue::open_waiting(&mut state, &facts("c1"));
            let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
            ready(&mut state, &clock);
            decision_queue::resolve_ticket(&mut state, &node, &artifact, false);
            let mut next = old;
            next.seq = 9;
            next.recorded_at_ms = 123;
            if changed {
                next.artifact = ArtifactId::from(ContentHash::parse_hex(&"b".repeat(64)).unwrap());
                next.envelope["request"]["params"]["message"] = json!("New question");
            }
            decision_queue::raise_ticket(&mut state, next.clone(), responder);
            let item = state.decisions.items().next().unwrap();
            assert!(!item.is_stale());
            let decision_queue::DecisionSubject::Ticket(actual) = &item.subject else {
                panic!("ticket")
            };
            assert_eq!(actual.artifact, next.artifact);
            assert_eq!(actual.envelope, next.envelope);
            assert_eq!((actual.seq, actual.recorded_at_ms), (9, 123));
            assert_ne!(
                route(&mut state, &special(DomainKey::Enter), &facts("c1")),
                Some(RouteEffect::Action(InputAction::Decision(
                    DecisionEffect::TicketAccept
                )))
            );
            ready(&mut state, &clock);
            assert_eq!(
                route(&mut state, &special(DomainKey::Enter), &facts("c1")),
                Some(RouteEffect::Action(InputAction::Decision(
                    DecisionEffect::TicketAccept
                )))
            );
        }
    }
}

#[test]
fn story21_6_ticket_serialized_enum_and_aggregate_bounds_apply_in_both_hosts() {
    let max = decision_queue::ANSWER_MAX_BYTES;
    for responder in [Responder::Local, Responder::Attached] {
        for aggregate in [false, true] {
            for extra in [0, 1] {
                let schema = if aggregate {
                    let overhead = serde_json::to_string(&json!({"a":"","b":""}))
                        .unwrap()
                        .len();
                    let a = "a".repeat((max - overhead) / 2);
                    let b = "b".repeat(max - overhead - a.len() + extra);
                    json!({"type":"object","properties":{
                        "a":{"type":"string","enum":[a]},
                        "b":{"type":"string","enum":[b]}}})
                } else {
                    json!({"type":"string","enum":["a".repeat(max - 2 + extra)]})
                };
                let (mut state, clock) = clocked(80, 24);
                decision_queue::raise_ticket(&mut state, ticket(schema), responder);
                decision_queue::open_waiting(&mut state, &facts("c1"));
                let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
                ready(&mut state, &clock);
                let action = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
                if extra == 0 {
                    assert_eq!(
                        action,
                        Some(RouteEffect::Action(InputAction::Decision(
                            DecisionEffect::TicketAccept
                        )))
                    );
                } else {
                    assert_ne!(
                        action,
                        Some(RouteEffect::Action(InputAction::Decision(
                            DecisionEffect::TicketAccept
                        )))
                    );
                    let painted = screen(&paint(&mut state, &facts("c1"), 80, 24));
                    assert!(painted.contains("serialized limit"), "{painted}");
                    assert_eq!(decision_queue::waiting(&state), 1);
                }
            }
        }
    }
}

#[test]
fn story21_6_unsupported_patterns_offer_raw_answer_in_both_hosts() {
    for responder in [Responder::Local, Responder::Attached] {
        for pattern in ["(?=a)a", "(a){100000000}"] {
            let (mut state, clock) = clocked(80, 24);
            let request = ticket(json!({"type":"object","properties":{
                "x":{"type":"string","pattern":pattern}}}));
            assert!(matches!(request.form, decision_queue::TicketForm::Raw(_)));
            decision_queue::raise_ticket(&mut state, request, responder);
            decision_queue::open_waiting(&mut state, &facts("c1"));
            let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
            ready(&mut state, &clock);
            let _ = route(&mut state, &key('e'), &facts("c1"));
            for ch in r#"{"x":"a"}"#.chars() {
                let _ = route(&mut state, &key(ch), &facts("c1"));
            }
            assert_eq!(
                route(&mut state, &special(DomainKey::Enter), &facts("c1")),
                Some(RouteEffect::Action(InputAction::Decision(
                    DecisionEffect::TicketAccept
                )))
            );
        }
    }
}
