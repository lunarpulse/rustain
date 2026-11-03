//! Story 21.4 A1: production-front-door ownership and compositor evidence.
#[path = "conformance_21_4_modal_stack/k2_boundary.rs"]
mod k2_boundary;
#[path = "conformance_21_4_modal_stack/k3_compose.rs"]
mod k3_compose;
#[path = "conformance_21_4_modal_stack/k4_geometry.rs"]
mod k4_geometry;
#[path = "conformance_21_4_modal_stack/k6_gate.rs"]
mod k6_gate;
use crate::conformance_21_3_keymap::support as keymap_support;

mod k1_stack {
    use super::keymap_support::press;
    use rustain::adapters::tui::{
        app::InputAction,
        decision_pane::{self, Arrival, DecisionDraft, DecisionKind},
        keymap::Key,
        notification_log::LogFacts,
        presentation,
        quit_confirm::{self, QuitFacts, QuitHost},
        state::TuiState,
    };

    #[test]
    fn esc_backs_exactly_one_frame_and_restores_the_displaced_one() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = "untouched".into();
        decision_pane::raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "reachable fifth-surface control"),
            Arrival::Automatic,
        );
        press(&mut state, Key::Ctrl('p'));
        press(&mut state, Key::Ctrl('x'));
        press(&mut state, Key::Char('?'));
        state.help_overlay.scroll_offset = 7;
        press(&mut state, Key::Ctrl('x'));
        press(&mut state, Key::Char('n'));
        press(&mut state, Key::Ctrl('x'));
        assert_eq!(
            press(&mut state, Key::Char('q')),
            InputAction::FocusDecisions
        );
        decision_pane::open_waiting(&mut state, &LogFacts::default());
        assert_eq!(state.modal_stack.len(), 4, "the pane must join the stack");
        let before: Vec<_> = state.modal_stack.iter().cloned().collect();
        let payload = state.notification_log.clone();
        assert_eq!(press(&mut state, Key::Ctrl('q')), InputAction::Quit);
        quit_confirm::request(
            &mut state,
            QuitFacts {
                streams: 1,
                ..QuitFacts::idle(QuitHost::Local)
            },
        );
        assert_eq!(state.modal_stack.len(), 4, "never a fifth live frame");
        press(&mut state, Key::Char('n'));
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            before
        );
        assert_eq!(state.notification_log, payload);
        assert_eq!(state.help_overlay.scroll_offset, 7);
        assert_eq!(state.input_buffer, "untouched");
        assert!(presentation::consistent(&state));
        // Ctrl+C is a back, even before the pane's inert window has elapsed.
        press(&mut state, Key::Ctrl('c'));
        assert_eq!(state.modal_stack.len(), 3);
        press(&mut state, Key::Esc);
        assert_eq!(state.modal_stack.len(), 2);
        assert_eq!(state.help_overlay.scroll_offset, 7);
        // Withdrawal while the pane is a non-interactive return continuation
        // must not resurrect the withdrawn queue item on Quit cancellation.
        for chord in ['n', 'q'] {
            press(&mut state, Key::Ctrl('x'));
            let action = press(&mut state, Key::Char(chord));
            if action == InputAction::FocusDecisions {
                decision_pane::open_waiting(&mut state, &LogFacts::default());
            }
        }
        assert_eq!(press(&mut state, Key::Ctrl('q')), InputAction::Quit);
        quit_confirm::request(
            &mut state,
            QuitFacts {
                streams: 1,
                ..QuitFacts::idle(QuitHost::Local)
            },
        );
        decision_pane::withdraw(&mut state, DecisionKind::Budget);
        assert!(presentation::consistent(&state));
        assert!(!presentation::is_open(
            &state,
            presentation::Surface::DecisionPane
        ));
        press(&mut state, Key::Char('n'));
        assert_eq!(state.modal_stack.len(), 3);
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            presentation::Surface::NotificationLog
        );
    }

    #[test]
    fn every_five_surface_open_order_keeps_the_cap_and_queue_identity() {
        fn orders(prefix: &mut Vec<u8>, remaining: &[u8], check: &mut impl FnMut(&[u8])) {
            if remaining.is_empty() {
                check(prefix);
            } else {
                for (index, next) in remaining.iter().enumerate() {
                    prefix.push(*next);
                    let rest: Vec<_> = remaining
                        .iter()
                        .enumerate()
                        .filter_map(|(i, n)| (i != index).then_some(*n))
                        .collect();
                    orders(prefix, &rest, check);
                    prefix.pop();
                }
            }
        }
        use decision_pane::{DecisionItem, Raised};
        use presentation::{Frame, Opened, Surface, reducer};

        fn queued() -> (TuiState, DecisionItem) {
            let mut state = TuiState::new(80, 24);
            state.input_buffer = "untouched".into();
            assert_eq!(
                decision_pane::raise(
                    &mut state,
                    DecisionDraft::new(DecisionKind::Budget, "queued identity"),
                    Arrival::Automatic,
                ),
                Raised::Waiting
            );
            let item = state.decisions.items().next().unwrap().clone();
            (state, item)
        }
        fn queue_identity(state: &TuiState, item: &DecisionItem) {
            assert_eq!(state.decisions.items().collect::<Vec<_>>(), vec![item]);
            assert_eq!(state.input_buffer, "untouched");
            assert!(state.modal_stack.len() <= 4);
            assert!(presentation::consistent(state));
            if presentation::is_open(state, Surface::DecisionPane) {
                assert_eq!(
                    decision_pane::current(state, &LogFacts::default()),
                    Some(item)
                );
            }
        }
        fn frames(state: &TuiState) -> Vec<Frame> {
            state.modal_stack.iter().cloned().collect()
        }
        fn keys(state: &mut TuiState, opener: u8) -> InputAction {
            match opener {
                0 => press(state, Key::Ctrl('p')),
                4 => press(state, Key::Ctrl('q')),
                1..=3 => {
                    assert_eq!(press(state, Key::Ctrl('x')), InputAction::Consumed);
                    press(state, Key::Char(['?', 'n', 'q'][usize::from(opener - 1)]))
                }
                _ => unreachable!(),
            }
        }
        fn quit(state: &mut TuiState) {
            assert_eq!(
                quit_confirm::request(
                    state,
                    QuitFacts {
                        streams: 1,
                        ..QuitFacts::idle(QuitHost::Local)
                    },
                ),
                quit_confirm::QuitRequest::Confirming
            );
        }
        fn opened(state: &TuiState, before: &[Frame], surface: Surface) {
            let after = frames(state);
            let retained = before.len().min(3);
            assert_eq!(&after[..retained], &before[..retained]);
            assert_eq!(after.len(), (before.len() + 1).min(4));
            let top = after.last().unwrap();
            assert_eq!(top.surface, surface);
            assert!(before.iter().all(|frame| frame.id < top.id));
            assert_eq!(top.decision.is_some(), surface == Surface::DecisionPane);
        }

        let surfaces = [
            Surface::CommandPalette,
            Surface::Help,
            Surface::NotificationLog,
            Surface::DecisionPane,
            Surface::QuitConfirm,
        ];
        let (mut reachable, mut blocked, mut reducer_only) = (0, 0, 0);
        orders(&mut Vec::new(), &[0, 1, 2, 3, 4], &mut |order| {
            let quit_last = order.last() == Some(&4);
            let (mut state, item) = queued();
            let mut before_quit = Vec::new();
            for &opener in order {
                let before = frames(&state);
                let quit_owned = before
                    .last()
                    .is_some_and(|f| f.surface == Surface::QuitConfirm);
                let action = keys(&mut state, opener);
                if quit_owned {
                    // These are blocked-key controls, NOT successful openings.
                    // In particular, the n of ^X n is Quit's explicit Stay key.
                    assert_eq!(action, InputAction::Consumed, "{order:?}: {opener}");
                    assert!(state.pending_leader.is_none());
                    if opener == 2 {
                        assert_eq!(frames(&state), before_quit, "{order:?}");
                    } else {
                        assert_eq!(frames(&state), before, "{order:?}");
                        assert_eq!(
                            state.modal_stack.top().unwrap().surface,
                            Surface::QuitConfirm
                        );
                    }
                } else {
                    let surface = surfaces[usize::from(opener)];
                    match opener {
                        0..=2 => assert_eq!(action, InputAction::Consumed, "{order:?}"),
                        3 => {
                            assert_eq!(action, InputAction::FocusDecisions, "{order:?}");
                            decision_pane::open_waiting(&mut state, &LogFacts::default());
                        }
                        4 => {
                            assert_eq!(action, InputAction::Quit, "{order:?}");
                            before_quit = before.clone();
                            quit(&mut state);
                        }
                        _ => unreachable!(),
                    }
                    opened(&state, &before, surface);
                    if before.len() == 4 {
                        // Only the 24 Quit-last front-door orders reach all five.
                        assert!(quit_last);
                        let displaced = before.last().unwrap();
                        assert!(!state.modal_stack.iter().any(|f| f.id == displaced.id));
                        assert!(presentation::is_open(&state, displaced.surface));
                        queue_identity(&state, &item);
                        assert_eq!(press(&mut state, Key::Char('n')), InputAction::Consumed);
                        assert_eq!(frames(&state), before, "restore exact IDs and decision key");
                    }
                }
                queue_identity(&state, &item);
            }
            while let Some(top) = state.modal_stack.top().cloned() {
                let before = frames(&state);
                assert_eq!(reducer::back(&mut state), Some(top.surface));
                assert_eq!(frames(&state), before[..before.len() - 1]);
                queue_identity(&state, &item);
            }
            if quit_last {
                reachable += 1;
                return;
            }
            blocked += 1;

            // Reducer/opening coverage ONLY: Quit consumes front-door opening
            // keys in these 96 orders, so this is not a user-reachable journey.
            let (mut state, item) = queued();
            for &opener in order {
                let before = frames(&state);
                let surface = surfaces[usize::from(opener)];
                match surface {
                    Surface::DecisionPane => {
                        assert!(decision_pane::focus_oldest(
                            &mut state,
                            &LogFacts::default()
                        ));
                    }
                    Surface::QuitConfirm => quit(&mut state),
                    _ => assert_eq!(
                        reducer::open(&mut state, surface),
                        if before.len() < 4 {
                            Opened::Pushed
                        } else if surface == Surface::CommandPalette {
                            Opened::Replaced
                        } else {
                            Opened::Continued
                        }
                    ),
                }
                opened(&state, &before, surface);
                queue_identity(&state, &item);
                if before.len() == 4 {
                    let displaced = before.last().unwrap();
                    assert!(!state.modal_stack.iter().any(|f| f.id == displaced.id));
                    assert_eq!(
                        presentation::is_open(&state, displaced.surface),
                        surface != Surface::CommandPalette
                    );
                    assert_eq!(reducer::back(&mut state), Some(surface));
                    if surface == Surface::CommandPalette {
                        assert_eq!(
                            frames(&state),
                            before[..3],
                            "drill-down discards displaced ID"
                        );
                    } else {
                        assert_eq!(
                            frames(&state),
                            before,
                            "continuation restores IDs and decision key"
                        );
                    }
                    queue_identity(&state, &item);
                }
            }
            reducer_only += 1;
        });
        assert_eq!((reachable, blocked, reducer_only), (24, 96, 96));
    }
}

mod k5_restore {
    use super::keymap_support::press;
    use ratatui::{Terminal, backend::TestBackend};
    use rustain::adapters::tui::{
        decision_pane::{self, Arrival, DecisionDraft, DecisionKind, Raised},
        keymap::{Host, Key},
        notification_log::LogFacts,
        notifications::{self, NoticeSource, NotificationStore},
        presentation::{self, Shell, Surface},
        state::TuiState,
    };
    use rustain::domain::{
        clock::MockClock,
        models::{FocusState, NotificationsConfig},
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    fn state() -> (TuiState, Arc<MockClock>) {
        let clock = Arc::new(MockClock::new(Instant::now()));
        let mut state = TuiState::new(80, 24);
        state.notifications =
            NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
        (state, clock)
    }

    fn arrival(state: &mut TuiState) -> Raised {
        decision_pane::raise(
            state,
            DecisionDraft::new(DecisionKind::Budget, "budget detail"),
            Arrival::Automatic,
        )
    }

    fn ready(state: &mut TuiState, clock: &MockClock) {
        let layout = super::k3_compose::layout(state);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut painted = false;
        terminal
            .draw(|frame| {
                painted = presentation::compose(
                    frame,
                    state,
                    &Shell::new(frame.area(), &layout),
                    Host::Local,
                    &LogFacts::default(),
                    |_| {},
                    |_| {},
                );
            })
            .unwrap();
        assert!(painted, "only a visible top pane can start its inert clock");
        decision_pane::note_painted(state);
        clock.advance(decision_pane::INERT_AFTER_PAINT);
    }

    #[test]
    fn arrival_restores_help_scroll_and_log_selection_by_id_exactly() {
        for log in [false, true] {
            let (mut state, clock) = state();
            if log {
                for text in ["older", "newer"] {
                    notifications::info(&mut state, NoticeSource::System, text);
                }
            }
            press(&mut state, Key::Ctrl('x'));
            press(&mut state, Key::Char(if log { 'n' } else { '?' }));
            press(&mut state, Key::Char('j'));
            assert_eq!(
                arrival(&mut state),
                Raised::Waiting,
                "recent inspector printable protects arrival"
            );
            decision_pane::withdraw(&mut state, DecisionKind::Budget);
            clock.advance(Duration::from_secs(1));
            let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
            let help = state.help_overlay.scroll_offset;
            let payload = state.notification_log.clone();
            assert_eq!(arrival(&mut state), Raised::Focused);
            assert_eq!(state.modal_stack.len(), 2);
            ready(&mut state, &clock);
            let action = press(&mut state, if log { Key::Char('y') } else { Key::Esc });
            if log {
                assert_eq!(
                    action,
                    rustain::adapters::tui::app::InputAction::Decision(
                        decision_pane::DecisionEffect::BudgetContinue,
                    )
                );
            }
            assert_eq!(
                state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                frames
            );
            assert_eq!(state.help_overlay.scroll_offset, help);
            assert_eq!(state.notification_log, payload);
            assert!(presentation::consistent(&state));
        }
    }

    #[test]
    fn arrival_never_preempts_a_typing_frame() {
        let (mut state, clock) = state();
        press(&mut state, Key::Ctrl('p'));
        for c in ['m', 'o'] {
            press(&mut state, Key::Char(c));
        }
        // Remove the independent recent-printable guard: this is the M6 killer.
        clock.advance(Duration::from_secs(2));
        let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
        let selection = state.command_palette.selected_index;
        assert_eq!(arrival(&mut state), Raised::Waiting);
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            frames
        );
        assert_eq!(state.command_palette.filter_text, "mo");
        assert_eq!(state.command_palette.selected_index, selection);
        assert!(
            state
                .notifications
                .records()
                .any(|r| r.message == decision_pane::waiting_toast())
        );
        assert_eq!(
            decision_pane::status_cue(decision_pane::waiting(&state)).as_deref(),
            Some("Needs you (1) ^X q")
        );
        let (mut idle, _) = self::state();
        idle.focus = FocusState::Chat;
        assert_eq!(
            arrival(&mut idle),
            Raised::Focused,
            "idle Chat positive control"
        );
    }

    #[test]
    fn each_arrival_guard_has_an_independent_control() {
        for guard in ["printable", "composer", "defer", "quit", "second"] {
            let (mut state, clock) = state();
            match guard {
                "printable" => {
                    press(&mut state, Key::Char('a'));
                    state.input_buffer.clear();
                }
                "composer" => {
                    state.input_buffer = "draft".into();
                    clock.advance(Duration::from_secs(2));
                }
                "defer" => {
                    assert_eq!(arrival(&mut state), Raised::Focused);
                    ready(&mut state, &clock);
                    press(&mut state, Key::Esc);
                    decision_pane::withdraw(&mut state, DecisionKind::Budget);
                }
                "quit" => {
                    super::k3_compose::open_quit(&mut state);
                }
                "second" => {
                    press(&mut state, Key::Char('a'));
                    assert_eq!(
                        decision_pane::raise(
                            &mut state,
                            DecisionDraft::new(DecisionKind::Carryover, "first"),
                            Arrival::Automatic,
                        ),
                        Raised::Waiting
                    );
                    state.input_buffer.clear();
                    clock.advance(Duration::from_secs(2));
                    assert!(state.modal_stack.is_empty());
                    assert!(!state.decisions.items().next().unwrap().deferred);
                    // Same printable/composer history and elapsed grace, but
                    // no queued item: no other guard can mask the 0→1 rule.
                    let (mut empty, empty_clock) = self::state();
                    press(&mut empty, Key::Char('a'));
                    empty.input_buffer.clear();
                    empty_clock.advance(Duration::from_secs(2));
                    assert_eq!(arrival(&mut empty), Raised::Focused);
                    assert_eq!(
                        empty.modal_stack.top().unwrap().surface,
                        Surface::DecisionPane
                    );
                }
                _ => unreachable!(),
            }
            let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
            assert_eq!(arrival(&mut state), Raised::Waiting, "{guard}");
            assert_eq!(
                state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                frames,
                "{guard}"
            );
            if guard == "second" {
                let items: Vec<_> = state.decisions.items().collect();
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].kind, DecisionKind::Carryover);
                assert_eq!(items[0].detail, "first");
                assert!(!items[0].deferred);
                assert_eq!(items[1].kind, DecisionKind::Budget);
            }
        }
        // The cooldown boundary is inclusive at 60 seconds.
        let (mut state, clock) = state();
        assert_eq!(arrival(&mut state), Raised::Focused);
        ready(&mut state, &clock);
        press(&mut state, Key::Esc);
        decision_pane::withdraw(&mut state, DecisionKind::Budget);
        clock.advance(Duration::from_secs(60));
        assert_eq!(arrival(&mut state), Raised::Focused);
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::DecisionPane
        );
    }
}
