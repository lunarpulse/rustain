//! K9 / AC18–AC21: confirmation effects, origin, deferral and stale targets.
use super::raisers::{self, ALL_6D, PLAIN, facts_6d};
use super::support::{clocked, key, paint, ready, route, screen, special, status_row};
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{self, DecisionKind, DecisionSubject};
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface};
use rustain::domain::events::AppEvent;
use rustain::domain::events::DomainKey;

fn action(kind: DecisionKind, yes: bool) -> InputAction {
    use DecisionKind::*;
    match (kind, yes) {
        (Consolidation, true) => InputAction::ConsolidateAcceptAll,
        (Consolidation, false) => InputAction::ConsolidateDeclineAll,
        (Forget, true) => InputAction::ForgetAcceptAll,
        (Forget, false) => InputAction::ForgetDeclineAll,
        (ArtifactApply | ArtifactResolve, true) => InputAction::ApplyCardAccept,
        (ArtifactApply | ArtifactResolve, false) => InputAction::ApplyCardDecline,
        (PeerAdd, true) => InputAction::PeerAddConfirm,
        (PeerAdd, false) => InputAction::PeerAddDecline,
        (TeamRetract, true) => InputAction::TeamRetractConfirm,
        (TeamRetract, false) => InputAction::TeamRetractDecline,
        (Fork, true) => InputAction::ForkConfirm,
        (Fork, false) => InputAction::ForkCancel,
        (Rewind, true) => InputAction::RewindConfirm,
        (Rewind, false) => InputAction::RewindCancel,
        (ExportOverwrite, true) => InputAction::ConfirmExportOverwrite,
        (ExportOverwrite, false) => InputAction::CancelExportOverwrite,
        _ => unreachable!(),
    }
}

#[test]
fn k9_every_kind_defers_and_both_answers_emit_once() {
    for kind in ALL_6D {
        let (mut state, clock) = clocked(80, 24);
        let base = state.focus.clone();
        raisers::raise(&mut state, kind, PLAIN);
        assert!(presentation::is_open(&state, Surface::Decision), "{kind:?}");
        paint(&mut state, &facts_6d("c1"), 80, 24);
        ready(&mut state, &clock);
        assert_eq!(
            route(&mut state, &special(DomainKey::Esc), &facts_6d("c1")),
            Some(RouteEffect::Consumed)
        );
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert_eq!(state.focus, base);
        assert_eq!(decision_queue::waiting(&state), 1);
        assert!(status_row(&state, 80).contains("Needs you (1) ^X q"));
        for yes in [true, false] {
            let (mut state, clock) = clocked(80, 24);
            raisers::raise(&mut state, kind, PLAIN);
            paint(&mut state, &facts_6d("c1"), 80, 24);
            let c = if yes { 'y' } else { 'n' };
            assert_eq!(
                route(&mut state, &key(c), &facts_6d("c1")),
                Some(RouteEffect::Consumed),
                "inert {kind:?}"
            );
            ready(&mut state, &clock);
            assert_eq!(
                route(&mut state, &key(c), &facts_6d("c1")),
                Some(RouteEffect::Action(action(kind, yes))),
                "{kind:?} {c}"
            );
            assert!(
                !action(kind, yes).is_mutation(),
                "{kind:?}: the host must deliver the answer even on a read-only tab"
            );
            assert!(
                state.decisions.take_resolved().is_some(),
                "{kind:?}: payload delivered"
            );
            assert!(
                state.decisions.take_resolved().is_none(),
                "{kind:?}: exactly once"
            );
            assert_ne!(
                route(&mut state, &key(c), &facts_6d("c1")),
                Some(RouteEffect::Action(action(kind, yes)))
            );
        }
    }
}

#[test]
fn k9_forget_navigation_resolves_selected_keys_only() {
    let (mut state, clock) = clocked(80, 24);
    raisers::raise(&mut state, DecisionKind::Forget, PLAIN);
    paint(&mut state, &facts_6d("c1"), 80, 24);
    ready(&mut state, &clock);
    for event in [
        key(' '),
        key('j'),
        special(DomainKey::Up),
        key('k'),
        special(DomainKey::Down),
    ] {
        assert_eq!(
            route(&mut state, &event, &facts_6d("c1")),
            Some(RouteEffect::Consumed)
        );
    }
    assert_eq!(
        route(&mut state, &key('y'), &facts_6d("c1")),
        Some(RouteEffect::Action(InputAction::ForgetAcceptAll))
    );
    let subject = state.decisions.take_resolved().expect("forget payload");
    assert!(
        matches!(decision_queue::memory_answer_event(subject, true), Some(AppEvent::MemoryForgetResolved { conversation_id, keys }) if conversation_id == "c1" && keys == vec![9])
    );
    assert!(state.decisions.take_resolved().is_none());
}

/// Exercise the public producers and the local event-loop's memory-answer emitter.
#[test]
fn k9_local_memory_answers_supply_the_host_event_values() {
    use rustain::domain::models::MemoryFact;

    for kind in [DecisionKind::Consolidation, DecisionKind::Forget] {
        for yes in [true, false] {
            let (mut state, clock) = clocked(80, 24);
            raisers::raise(&mut state, kind, PLAIN);
            paint(&mut state, &facts_6d("c1"), 80, 24);
            ready(&mut state, &clock);
            let answer = action(kind, yes);
            assert!(!answer.is_mutation());
            assert_eq!(
                route(
                    &mut state,
                    &key(if yes { 'y' } else { 'n' }),
                    &facts_6d("c1")
                ),
                Some(RouteEffect::Action(answer))
            );
            let subject = state
                .decisions
                .take_resolved()
                .expect("resolved memory payload");
            let event = decision_queue::memory_answer_event(subject, yes);
            match (kind, event) {
                (
                    DecisionKind::Consolidation,
                    Some(AppEvent::MemoryConsolidationResolved {
                        conversation_id,
                        accepted,
                    }),
                ) => {
                    let expected = if yes {
                        vec![MemoryFact {
                            category: "Preferences".into(),
                            fact: PLAIN.into(),
                            detail: None,
                        }]
                    } else {
                        vec![]
                    };
                    assert_eq!(conversation_id, "c1");
                    assert_eq!(accepted, expected);
                }
                (
                    DecisionKind::Forget,
                    Some(AppEvent::MemoryForgetResolved {
                        conversation_id,
                        keys,
                    }),
                ) => {
                    assert_eq!(conversation_id, "c1");
                    assert_eq!(keys, if yes { vec![7, 9] } else { vec![] });
                }
                (_, other) => panic!("unexpected memory event: {other:?}"),
            }
            assert!(state.decisions.take_resolved().is_none());
        }
    }
}

#[test]
fn k9_memory_answer_event_rejects_non_memory_subjects() {
    for yes in [true, false] {
        assert!(decision_queue::memory_answer_event(DecisionSubject::Text, yes).is_none());
    }
}

fn stale_target(kind: DecisionKind) {
    for (messages, stale) in [(1, true), (2, false)] {
        let (mut state, clock) = clocked(80, 24);
        raisers::raise(&mut state, kind, PLAIN);
        let f = rustain::adapters::tui::notification_log::LogFacts {
            active_messages: messages,
            ..facts_6d("c1")
        };
        paint(&mut state, &f, 80, 24);
        ready(&mut state, &clock);
        let result = route(&mut state, &key('y'), &f);
        if stale {
            assert_eq!(result, Some(RouteEffect::Consumed));
            let text = screen(&paint(&mut state, &f, 80, 24));
            assert!(
                text.contains("This decision is no longer pending."),
                "{text}"
            );
            assert!(text.contains("[Esc] Close"), "{text}");
            assert!(!text.contains("[y]"), "{text}");
            assert!(state.decisions.take_resolved().is_none());
            assert_eq!(
                route(&mut state, &special(DomainKey::Esc), &f),
                Some(RouteEffect::Consumed)
            );
            assert!(state.decisions.is_empty());
        } else {
            assert_eq!(result, Some(RouteEffect::Action(action(kind, true))));
            assert!(state.decisions.take_resolved().is_some());
        }
    }
}

#[test]
fn a_stale_fork_target_closes_and_never_forks() {
    stale_target(DecisionKind::Fork);
}
#[test]
fn a_stale_rewind_target_closes_and_never_rewinds() {
    stale_target(DecisionKind::Rewind);
}

#[tokio::test]
async fn k9_another_tab_cannot_answer_origin_but_origin_can() {
    for kind in ALL_6D {
        let (mut state, clock) = clocked(80, 24);
        let temp = tempfile::tempdir().unwrap();
        if kind == DecisionKind::ExportOverwrite {
            raisers::raise_export_command(&mut state, temp.path()).await;
        } else {
            raisers::raise(&mut state, kind, PLAIN);
        }
        paint(&mut state, &facts_6d("c2"), 80, 24);
        ready(&mut state, &clock);
        for c in ['y', 'n'] {
            let result = route(&mut state, &key(c), &facts_6d("c2"));
            assert!(
                !matches!(&result, Some(RouteEffect::Action(_))),
                "{kind:?} {c}: {result:?}"
            );
            assert!(state.decisions.take_resolved().is_none(), "{kind:?} {c}");
            assert_eq!(decision_queue::waiting(&state), 1, "{kind:?} {c}");
        }
        let _ = route(&mut state, &special(DomainKey::Esc), &facts_6d("c2"));
        assert_eq!(
            decision_queue::waiting(&state),
            1,
            "{kind:?}: another tab cannot abandon it"
        );
        assert!(state.decisions.take_resolved().is_none());
        let queued = state.decisions.items().next().unwrap();
        let item = decision_queue::DecisionKey {
            kind: queued.kind,
            source: queued.source.clone(),
            conversation_id: queued.conversation_id.clone(),
        };
        assert!(decision_queue::open(&mut state, &item));
        paint(&mut state, &facts_6d("c1"), 80, 24);
        ready(&mut state, &clock);
        assert_eq!(
            route(&mut state, &key('y'), &facts_6d("c1")),
            Some(RouteEffect::Action(action(kind, true))),
            "{kind:?}"
        );
        assert!(state.decisions.take_resolved().is_some());
        assert!(
            state.decisions.take_resolved().is_none(),
            "{kind:?}: exactly once"
        );
    }
}

#[test]
fn k9_same_source_memory_items_keep_their_distinct_origins() {
    use decision_queue::PendingConsolidationCard;
    use rustain::domain::models::MemoryFact;

    let (mut state, clock) = clocked(80, 24);
    for origin in ["c1", "c2"] {
        decision_queue::raise_consolidation(
            &mut state,
            PendingConsolidationCard {
                conversation_id: origin.into(),
                proposals: vec![(
                    MemoryFact {
                        category: "Preferences".into(),
                        fact: origin.into(),
                        detail: None,
                    },
                    true,
                )],
            },
        );
    }
    let keys: Vec<_> = state
        .decisions
        .items()
        .map(|item| decision_queue::DecisionKey {
            kind: item.kind,
            source: item.source.clone(),
            conversation_id: item.conversation_id.clone(),
        })
        .collect();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].source, keys[1].source);
    for origin in ["c1", "c2"] {
        let item = keys
            .iter()
            .find(|key| key.conversation_id.as_deref() == Some(origin))
            .unwrap();
        assert!(decision_queue::open(&mut state, item));
        paint(&mut state, &facts_6d(origin), 80, 24);
        ready(&mut state, &clock);
        assert_eq!(
            route(&mut state, &key('y'), &facts_6d(origin)),
            Some(RouteEffect::Action(InputAction::ConsolidateAcceptAll))
        );
        let Some(DecisionSubject::Consolidation(payload)) = state.decisions.take_resolved() else {
            panic!("consolidation payload for {origin}");
        };
        assert_eq!(payload.card.conversation_id, origin);
        assert_eq!(payload.card.proposals[0].0.fact, origin);
        assert!(state.decisions.take_resolved().is_none());
    }
}

#[test]
fn k9_requested_arrival_respects_text_and_quit_but_opens_over_help() {
    use rustain::adapters::tui::quit_confirm::{QuitFacts, QuitHost};
    for kind in ALL_6D
        .into_iter()
        .filter(|k| *k != DecisionKind::Consolidation)
    {
        for case in ["input", "chat", "help", "palette", "quit"] {
            let (mut state, clock) = clocked(80, 24);
            match case {
                "chat" => state.focus = rustain::domain::models::FocusState::Chat,
                "help" => {
                    presentation::reducer::open(&mut state, Surface::Help);
                }
                "palette" => {
                    presentation::reducer::open(&mut state, Surface::CommandPalette);
                }
                "quit" => {
                    let mut quit = QuitFacts::idle(QuitHost::Local);
                    quit.composer_draft = true;
                    presentation::reducer::request_quit(&mut state, quit);
                }
                _ => {}
            }
            let stack: Vec<_> = state.modal_stack.iter().cloned().collect();
            raisers::raise(&mut state, kind, PLAIN);
            if matches!(case, "palette" | "quit") {
                assert!(
                    !presentation::is_open(&state, Surface::Decision),
                    "{kind:?} {case}"
                );
                assert_eq!(state.modal_stack.iter().cloned().collect::<Vec<_>>(), stack);
                assert!(
                    state
                        .notifications
                        .visible()
                        .any(|n| n.message == decision_queue::waiting_toast()),
                    "{kind:?} {case}"
                );
                assert!(state.decisions.take_resolved().is_none());
            } else {
                assert!(
                    presentation::is_open(&state, Surface::Decision),
                    "{kind:?} {case}"
                );
                paint(&mut state, &facts_6d("c1"), 80, 24);
                ready(&mut state, &clock);
                assert_eq!(
                    route(&mut state, &key('n'), &facts_6d("c1")),
                    Some(RouteEffect::Action(action(kind, false)))
                );
                assert!(state.decisions.take_resolved().is_some());
                assert_eq!(state.modal_stack.iter().cloned().collect::<Vec<_>>(), stack);
            }
        }
    }
}

#[tokio::test]
async fn k9_export_real_command_detects_changed_file_and_preserves_it() {
    use rustain::adapters::tui::handlers::export::apply_confirm_export_overwrite;
    for changed in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (mut state, clock) = clocked(80, 24);
        let path = raisers::raise_export_command(&mut state, temp.path()).await;
        paint(&mut state, &facts_6d("c1"), 80, 24);
        ready(&mut state, &clock);
        if changed {
            std::fs::write(&path, "externally changed").unwrap();
        }
        assert_eq!(
            route(&mut state, &key('y'), &facts_6d("c1")),
            Some(RouteEffect::Action(InputAction::ConfirmExportOverwrite))
        );
        let Some(DecisionSubject::ExportOverwrite(payload)) = state.decisions.take_resolved()
        else {
            panic!("export payload")
        };
        apply_confirm_export_overwrite(&mut state, *payload).await;
        let contents = std::fs::read_to_string(&path).unwrap();
        if changed {
            assert_eq!(contents, "externally changed");
            assert!(
                state
                    .notifications
                    .records()
                    .any(|n| n.message.contains("file changed"))
            );
        } else {
            assert_ne!(contents, "old");
        }
        assert!(state.decisions.take_resolved().is_none());
    }
}

/// 6D-L1-06: wrapped earlier summaries must not hide the focused, toggleable memory.
#[test]
fn k9_forget_navigation_reveals_the_focused_memory_after_wrapping() {
    use rustain::adapters::tui::decision_queue::PendingForgetCard;
    use rustain::domain::models::MemoryEntry;
    for (width, height) in [(60, 16), (80, 24)] {
        let (mut state, clock) = clocked(width, height);
        let candidates = (0..5)
            .map(|i| {
                let summary = if i == 0 {
                    "first ".repeat(500)
                } else {
                    format!("memory {i}")
                };
                (
                    i + 1,
                    MemoryEntry {
                        timestamp: chrono::Local::now(),
                        summary,
                        context: None,
                    },
                    true,
                )
            })
            .collect();
        decision_queue::raise_forget(
            &mut state,
            PendingForgetCard {
                conversation_id: "c1".into(),
                candidates,
                focused_index: 0,
            },
        );
        paint(&mut state, &facts_6d("c1"), width, height);
        ready(&mut state, &clock);
        for _ in 0..4 {
            assert_eq!(
                route(&mut state, &special(DomainKey::Down), &facts_6d("c1")),
                Some(RouteEffect::Consumed)
            );
        }
        let rendered = screen(&paint(&mut state, &facts_6d("c1"), width, height));
        assert!(
            rendered
                .lines()
                .any(|row| row.contains("> ") && row.contains("[0000000000000005]")),
            "{width}x{height}: {rendered}"
        );
        let (new_width, new_height) = if width == 60 { (80, 24) } else { (60, 16) };
        state.terminal_width = new_width;
        let resized = screen(&paint(&mut state, &facts_6d("c1"), new_width, new_height));
        assert!(
            resized
                .lines()
                .any(|row| row.contains("> ") && row.contains("[0000000000000005]")),
            "{new_width}x{new_height}: {resized}"
        );
        state.terminal_width = width;
        assert_eq!(
            route(&mut state, &key(' '), &facts_6d("c1")),
            Some(RouteEffect::Consumed)
        );
        let DecisionSubject::Forget(card) = &state.decisions.items().next().unwrap().subject else {
            panic!("forget payload")
        };
        assert_eq!(
            card.candidates
                .iter()
                .map(|(key, _, selected)| (*key, *selected))
                .collect::<Vec<_>>(),
            vec![(1, true), (2, true), (3, true), (4, true), (5, false)]
        );
        for _ in 0..4 {
            assert_eq!(
                route(&mut state, &special(DomainKey::Up), &facts_6d("c1")),
                Some(RouteEffect::Consumed)
            );
        }
        let rendered = screen(&paint(&mut state, &facts_6d("c1"), width, height));
        assert!(
            rendered
                .lines()
                .any(|row| row.contains("> ") && row.contains("first")),
            "{width}x{height}: {rendered}"
        );
    }
}
