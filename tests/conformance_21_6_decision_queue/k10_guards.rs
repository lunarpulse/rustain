//! K10 / AC18–AC20: token, retract, artifact and raw-key guards.
use super::raisers::{self, PLAIN, facts_6d};
use super::support::{clocked, key, paint, ready, route, screen};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rustain::adapters::daemon::protocol::{ClientFrame, ProposalToken};
use rustain::adapters::tui::app::{self, InputAction};
use rustain::adapters::tui::decision_queue::{
    self, ConsolidationPayload, DecisionKind, DecisionSubject, PendingConsolidationCard,
};
use rustain::adapters::tui::presentation::RouteEffect;
use rustain::adapters::tui::widgets::apply_card::{ApplyCardChoice, choice_for_key};

#[test]
fn mismatched_consolidation_token_never_resolves() {
    let payload = ConsolidationPayload {
        card: PendingConsolidationCard {
            conversation_id: String::new(),
            proposals: vec![],
        },
        token: Some(ProposalToken(8)),
    };
    for accept in [true, false] {
        assert!(
            matches!(decision_queue::consolidation_frame("8", &payload, accept), Some(ClientFrame::ConsolidationResolve { token: ProposalToken(8), accept: got }) if got == accept)
        );
        assert!(decision_queue::consolidation_frame("7", &payload, accept).is_none());
        assert!(
            decision_queue::consolidation_frame(
                "8",
                &ConsolidationPayload {
                    token: None,
                    ..payload.clone()
                },
                accept
            )
            .is_none()
        );
    }
}

#[test]
fn attached_consolidations_keep_their_own_tokens() {
    let (mut state, _) = clocked(80, 24);
    for token in [ProposalToken(8), ProposalToken(9)] {
        decision_queue::raise_attached_consolidation(&mut state, token, vec![]);
    }
    let items: Vec<_> = state.decisions.items().collect();
    assert_eq!(items.len(), 2);
    for (item, expected) in items.iter().zip([8, 9]) {
        let DecisionSubject::Consolidation(payload) = &item.subject else {
            panic!("consolidation payload")
        };
        assert_eq!(item.source, expected.to_string());
        assert!(
            matches!(decision_queue::consolidation_frame(&item.source, payload, true), Some(ClientFrame::ConsolidationResolve { token: ProposalToken(t), accept: true }) if t == expected)
        );
        assert!(
            decision_queue::consolidation_frame(&(17 - expected).to_string(), payload, true)
                .is_none()
        );
    }
}

#[test]
fn disarmed_retract_cannot_confirm_armed_retract_confirms_once() {
    for armed in [false, true] {
        let (mut state, clock) = clocked(80, 24);
        raisers::raise_retract(&mut state, PLAIN, armed);
        let text = screen(&paint(&mut state, &facts_6d("c1"), 80, 24));
        assert!(text.contains("[n] Cancel"), "{text}");
        assert_eq!(text.contains("[y] Retract"), armed, "{text}");
        ready(&mut state, &clock);
        let result = route(&mut state, &key('y'), &facts_6d("c1"));
        if armed {
            assert_eq!(
                result,
                Some(RouteEffect::Action(InputAction::TeamRetractConfirm))
            );
            assert!(state.decisions.take_resolved().is_some());
            assert!(state.decisions.take_resolved().is_none());
        } else {
            assert_eq!(result, Some(RouteEffect::Consumed));
            assert!(state.decisions.take_resolved().is_none());
            assert_eq!(
                route(&mut state, &key('n'), &facts_6d("c1")),
                Some(RouteEffect::Action(InputAction::TeamRetractDecline))
            );
        }
    }
}

#[test]
fn artifact_choices_match_legacy_table_and_mode_rails() {
    for (kind, rail) in [
        (DecisionKind::ArtifactApply, "[y] Apply  [n] Cancel"),
        (
            DecisionKind::ArtifactResolve,
            "[y] Record report  [n] Cancel",
        ),
    ] {
        for byte in 32u8..=126 {
            let c = char::from(byte);
            let (mut state, clock) = clocked(80, 24);
            raisers::raise(&mut state, kind, PLAIN);
            let text = screen(&paint(&mut state, &facts_6d("c1"), 80, 24));
            assert!(text.contains(rail), "{kind:?}: {text}");
            ready(&mut state, &clock);
            let result = route(&mut state, &key(c), &facts_6d("c1"));
            match choice_for_key(c) {
                Some(ApplyCardChoice::Accept) => assert_eq!(
                    result,
                    Some(RouteEffect::Action(InputAction::ApplyCardAccept))
                ),
                Some(ApplyCardChoice::Decline) => assert_eq!(
                    result,
                    Some(RouteEffect::Action(InputAction::ApplyCardDecline))
                ),
                None => assert!(
                    !matches!(
                        result,
                        Some(RouteEffect::Action(
                            InputAction::ApplyCardAccept | InputAction::ApplyCardDecline
                        ))
                    ),
                    "{kind:?} {c:?}: {result:?}"
                ),
            }
        }
    }
}

#[test]
fn raw_deciding_keys_require_plain_press_in_each_new_context() {
    for (kind, chars) in [
        (DecisionKind::Forget, "yn"),
        (DecisionKind::Rewind, "yfn"),
        (DecisionKind::ExportOverwrite, "yn"),
        (DecisionKind::Fork, "yn"),
        (DecisionKind::Consolidation, "yn"),
        (DecisionKind::ArtifactApply, "yn"),
        (DecisionKind::ArtifactResolve, "yn"),
        (DecisionKind::PeerAdd, "yn"),
        (DecisionKind::TeamRetract, "yn"),
    ] {
        for c in chars.chars() {
            for (modifiers, event_kind) in [
                (KeyModifiers::NONE, KeyEventKind::Repeat),
                (KeyModifiers::NONE, KeyEventKind::Release),
                (KeyModifiers::SHIFT, KeyEventKind::Press),
                (KeyModifiers::ALT, KeyEventKind::Press),
                (KeyModifiers::CONTROL, KeyEventKind::Press),
            ] {
                let (mut state, _, _) = raisers::open_ready(kind, PLAIN);
                let raw = Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char(c),
                    modifiers,
                    event_kind,
                ));
                assert_eq!(
                    app::handle_operator_input(&mut state, &raw, &key(c)),
                    None,
                    "{kind:?} {c} {modifiers:?} {event_kind:?}"
                );
                assert!(state.decisions.take_resolved().is_none());
            }
            let (mut state, _, _) = raisers::open_ready(kind, PLAIN);
            let raw = Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char(c),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            ));
            // The wrapper exposes the raw guard but uses default LogFacts internally;
            // probe the guard with a non-key domain event, then route the real key
            // through the fact-aware boundary below.
            assert!(
                app::handle_operator_input(
                    &mut state,
                    &raw,
                    &rustain::domain::events::DomainInputEvent::FocusGained
                )
                .is_some(),
                "plain {kind:?} {c}"
            );
            // The default-facts wrapper can heal the frame closed; use a fresh
            // equivalent state for the fact-aware positive-control dispatch.
            let (mut state, _, _) = raisers::open_ready(kind, PLAIN);
            assert!(
                matches!(
                    route(&mut state, &key(c), &facts_6d("c1")),
                    Some(RouteEffect::Action(_))
                ),
                "plain {kind:?} {c}"
            );
            assert!(state.decisions.take_resolved().is_some());
        }
    }
}

#[test]
fn f_cannot_decide_any_context_without_a_fork_instead_binding() {
    for kind in [
        DecisionKind::Forget,
        DecisionKind::ExportOverwrite,
        DecisionKind::Fork,
        DecisionKind::Consolidation,
        DecisionKind::ArtifactApply,
        DecisionKind::ArtifactResolve,
        DecisionKind::PeerAdd,
        DecisionKind::TeamRetract,
    ] {
        for (modifiers, event_kind) in [
            (KeyModifiers::NONE, KeyEventKind::Press),
            (KeyModifiers::NONE, KeyEventKind::Repeat),
            (KeyModifiers::NONE, KeyEventKind::Release),
            (KeyModifiers::SHIFT, KeyEventKind::Press),
            (KeyModifiers::ALT, KeyEventKind::Press),
            (KeyModifiers::CONTROL, KeyEventKind::Press),
        ] {
            let (mut state, _, _) = raisers::open_ready(kind, PLAIN);
            let raw = Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('f'),
                modifiers,
                event_kind,
            ));
            let domain = if modifiers.is_empty() {
                key('f')
            } else {
                rustain::domain::events::DomainInputEvent::FocusGained
            };
            let got = app::handle_operator_input(&mut state, &raw, &domain);
            assert!(
                !matches!(
                    got,
                    Some(
                        InputAction::ForkConfirm
                            | InputAction::RewindForkInstead
                            | InputAction::ForgetAcceptAll
                            | InputAction::ApplyCardAccept
                            | InputAction::PeerAddConfirm
                            | InputAction::TeamRetractConfirm
                            | InputAction::ConfirmExportOverwrite
                            | InputAction::ConsolidateAcceptAll
                    )
                ),
                "{kind:?} {modifiers:?} {event_kind:?}"
            );
            assert!(state.decisions.take_resolved().is_none());
        }
        let (mut state, _, _) = raisers::open_ready(kind, PLAIN);
        assert!(
            matches!(
                route(&mut state, &key('y'), &facts_6d("c1")),
                Some(RouteEffect::Action(_))
            ),
            "{kind:?}: positive control"
        );
    }
}

#[test]
fn peer_add_uppercase_is_not_a_confirmation_key() {
    for c in ['Y', 'N'] {
        let (mut state, _, _) = raisers::open_ready(DecisionKind::PeerAdd, PLAIN);
        assert_eq!(
            route(&mut state, &key(c), &facts_6d("c1")),
            Some(RouteEffect::Consumed)
        );
        assert!(state.decisions.take_resolved().is_none());
        assert_eq!(
            route(&mut state, &key('y'), &facts_6d("c1")),
            Some(RouteEffect::Action(InputAction::PeerAddConfirm))
        );
        assert!(state.decisions.take_resolved().is_some());
    }
}
