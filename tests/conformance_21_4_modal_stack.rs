//! Story 21.4 A1: production-front-door ownership and compositor evidence.
#[path = "conformance_21_4_modal_stack/k10_text_field.rs"]
mod k10_text_field;
#[path = "conformance_21_4_modal_stack/k12_legacy.rs"]
mod k12_legacy;
#[path = "conformance_21_4_modal_stack/k2_boundary.rs"]
mod k2_boundary;
#[path = "conformance_21_4_modal_stack/k3_compose.rs"]
mod k3_compose;
#[path = "conformance_21_4_modal_stack/k4_geometry.rs"]
mod k4_geometry;
#[path = "conformance_21_4_modal_stack/k6_gate.rs"]
mod k6_gate;
#[path = "conformance_21_4_modal_stack/k7_cross_search.rs"]
mod k7_cross_search;
#[path = "conformance_21_4_modal_stack/k7_smoke_frames.rs"]
mod k7_smoke_frames;
#[path = "conformance_21_4_modal_stack/k9_reconcile.rs"]
mod k9_reconcile;
#[path = "conformance_21_4_modal_stack/stage_support.rs"]
mod stage_support;
use crate::conformance_21_3_keymap::support as keymap_support;

mod k1_stack {
    use super::keymap_support::press;
    use rustain::adapters::tui::{
        app::InputAction,
        decision_queue::{self, Arrival, DecisionDraft, DecisionKind},
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
        decision_queue::raise(
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
        decision_queue::open_waiting(&mut state, &LogFacts::default());
        assert_eq!(press(&mut state, Key::Enter), InputAction::Consumed);
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
                decision_queue::open_waiting(&mut state, &LogFacts::default());
                assert_eq!(press(&mut state, Key::Enter), InputAction::Consumed);
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
        decision_queue::withdraw(&mut state, DecisionKind::Budget);
        // old -> new: withdrawing the item whose frame was a Quit continuation
        // closed that frame; now an open frame is never pulled away (A7): the
        // item turns stale, its frame stays suspended, and nothing resurrects it.
        assert!(presentation::consistent(&state));
        assert!(presentation::is_open(
            &state,
            presentation::Surface::Decision
        ));
        assert_eq!(state.decisions.len(), 1);
        assert!(state.decisions.items().all(|item| item.is_stale()));
        press(&mut state, Key::Char('n'));
        assert_eq!(state.modal_stack.len(), 4);
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            presentation::Surface::Decision
        );
        assert_eq!(
            press(&mut state, Key::Char('y')),
            InputAction::Consumed,
            "a stale item decides nothing"
        );
        assert!(state.decisions.items().all(|item| item.is_stale()));
        assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
        assert!(state.decisions.is_empty(), "Close removes the stale item");
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
        use decision_queue::{DecisionItem, Raised};
        use presentation::{Frame, Opened, Surface, reducer};

        fn queued() -> (TuiState, DecisionItem) {
            let mut state = TuiState::new(80, 24);
            state.input_buffer = "untouched".into();
            assert_eq!(
                decision_queue::raise(
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
            if presentation::is_open(state, Surface::Decision) {
                assert_eq!(
                    decision_queue::current(state, &LogFacts::default()),
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
            assert_eq!(top.decision.is_some(), surface == Surface::Decision);
        }

        let surfaces = [
            Surface::CommandPalette,
            Surface::Help,
            Surface::NotificationLog,
            Surface::DecisionQueue,
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
                            decision_queue::open_waiting(&mut state, &LogFacts::default());
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
                    Surface::DecisionQueue => {
                        decision_queue::open_waiting(&mut state, &LogFacts::default());
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
        decision_queue::{self, Arrival, DecisionDraft, DecisionKind, Raised},
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

    pub(super) fn state() -> (TuiState, Arc<MockClock>) {
        let clock = Arc::new(MockClock::new(Instant::now()));
        let mut state = TuiState::new(80, 24);
        state.notifications =
            NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
        (state, clock)
    }

    pub(super) fn arrival(state: &mut TuiState) -> Raised {
        decision_queue::raise(
            state,
            DecisionDraft::new(DecisionKind::Budget, "budget detail"),
            Arrival::Automatic,
        )
    }

    pub(super) fn ready(state: &mut TuiState, clock: &MockClock) {
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
                    &mut presentation::occlusion::OpaqueRects::new(layout.chat_pane),
                    |_| {},
                );
            })
            .unwrap();
        assert!(painted, "only a visible top pane can start its inert clock");
        decision_queue::note_painted(state);
        clock.advance(decision_queue::INERT_AFTER_PAINT);
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
            decision_queue::withdraw(&mut state, DecisionKind::Budget);
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
                        decision_queue::DecisionEffect::BudgetContinue,
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
        assert_eq!(state.command_palette.filter_text.as_str(), "mo");
        assert_eq!(state.command_palette.selected_index, selection);
        assert!(
            state
                .notifications
                .records()
                .any(|r| r.message == decision_queue::waiting_toast())
        );
        assert_eq!(
            decision_queue::status_cue(decision_queue::waiting(&state)).as_deref(),
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
                    decision_queue::withdraw(&mut state, DecisionKind::Budget);
                }
                "quit" => {
                    super::k3_compose::open_quit(&mut state);
                }
                "second" => {
                    press(&mut state, Key::Char('a'));
                    assert_eq!(
                        decision_queue::raise(
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
                    assert_eq!(empty.modal_stack.top().unwrap().surface, Surface::Decision);
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
        decision_queue::withdraw(&mut state, DecisionKind::Budget);
        clock.advance(Duration::from_secs(60));
        assert_eq!(arrival(&mut state), Raised::Focused);
        assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Decision);
    }
}

mod k7_surfaces {
    use super::{k3_compose, k5_restore, keymap_support::press};
    use rustain::adapters::{
        noop::NoOpUsageLedger,
        tui::{
            app::InputAction,
            clipboard::ClipboardResult,
            handlers::{accounts, login, model_switch, profile_switch, usage_panel},
            keymap::Key,
            presentation::{self, Surface},
            state::TuiState,
        },
    };
    use rustain::domain::{
        errors::ProviderError,
        models::{
            AppConfig, FocusState, ProfileDescriptor, ProfileIdentityColor, ProfileSource,
            SessionManager, SessionState, StreamingState,
            palette::{PaletteAction, PaletteEntry, PaletteScope},
            provider::{LockedProvider, ModelDescriptor, ProviderDescriptor},
        },
        ports::{ProviderInfoPort, StreamingProvider},
    };
    use std::{sync::Arc, time::Duration};

    pub(super) struct Info;
    impl Info {
        fn model(id: &str) -> ModelDescriptor {
            ModelDescriptor {
                model_id: id.into(),
                display_name: format!("Model {id}"),
                provider_id: "fixture".into(),
                context_window: 32_000,
                capabilities: Default::default(),
                pricing_tier: None,
                stale: false,
                wire_model_id: None,
            }
        }
    }
    impl ProviderInfoPort for Info {
        fn active_delegate_id(&self) -> String {
            "fixture".into()
        }
        fn get_model(&self, p: &str, m: &str) -> Option<ModelDescriptor> {
            (p == "fixture").then(|| Self::model(m))
        }
        fn get_model_provider(&self, _: &str, _: Option<&str>) -> Option<String> {
            Some("fixture".into())
        }
        fn list_providers(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor {
                provider_id: "fixture".into(),
                healthy: true,
                model_count: 2,
                display_name: "Fixture".into(),
            }]
        }
        fn list_models_by_provider(&self, _: &str) -> Vec<ModelDescriptor> {
            vec![Self::model("one"), Self::model("two")]
        }
        fn list_locked_providers(&self) -> Vec<LockedProvider> {
            vec![LockedProvider {
                provider_id: "anthropic".into(),
                display_name: "Anthropic".into(),
                sign_in_hint: "Run `rustain auth login anthropic`".into(),
                models: vec![],
            }]
        }
        fn get_provider(&self, _: &str) -> Option<Arc<dyn StreamingProvider>> {
            None
        }
        fn set_active_provider(&self, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
        fn now_unix(&self) -> i64 {
            0
        }
        fn today_start_unix_ms(&self) -> i64 {
            0
        }
    }
    fn profiles() -> Vec<ProfileDescriptor> {
        ["one", "two"]
            .map(|name| ProfileDescriptor {
                name: name.into(),
                description: None,
                preview: false,
                identity_color: ProfileIdentityColor(2),
                source: ProfileSource::Builtin,
                source_origin: None,
                selection: Default::default(),
            })
            .to_vec()
    }
    fn login_action(state: &mut TuiState, action: login::LoginAction) {
        let copier: rustain::adapters::tui::login_runtime::Copier =
            Arc::new(|_: &str| ClipboardResult::Osc52Success);
        login::on_action(state, &copier, &AppConfig::default(), &Info, action);
    }
    pub(super) async fn open(state: &mut TuiState, surface: Surface) {
        let key = match surface {
            Surface::ModelSelector => 'm',
            Surface::ProfileSwitcher => 'p',
            Surface::UsagePanel => 'u',
            Surface::Login => 'i',
            _ => unreachable!(),
        };
        assert_eq!(press(state, Key::Ctrl('x')), InputAction::Consumed);
        let action = press(state, Key::Char(key));
        match surface {
            Surface::ModelSelector => {
                assert_eq!(action, InputAction::OpenModelSelector);
                model_switch::open_selector(state, &Info, &AppConfig::default(), None);
            }
            Surface::ProfileSwitcher => {
                assert_eq!(action, InputAction::OpenProfileSwitcher);
                profile_switch::open(state, |selector| {
                    selector.initialize(profiles(), "one", Default::default());
                });
            }
            Surface::UsagePanel => {
                assert_eq!(action, InputAction::OpenUsagePanel);
                let conversation = crate::e2e_harness::TestHarness::new().conversation;
                tokio::time::timeout(
                    Duration::from_secs(2),
                    usage_panel::handle_open_usage_panel(
                        state,
                        &conversation,
                        &NoOpUsageLedger,
                        &Info,
                        &AppConfig::default(),
                        &SessionManager::new(SessionState::Empty),
                    ),
                )
                .await
                .expect("bounded ledger opener");
            }
            Surface::Login => {
                let InputAction::Login(action) = action else {
                    panic!("Login leader effect")
                };
                login_action(state, action);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            surface,
            "opener must be reachable"
        );
        assert!(presentation::consistent(state));
    }
    pub(super) fn back(state: &mut TuiState) {
        let action = press(state, Key::Esc);
        if let InputAction::Login(action) = action {
            login_action(state, action);
        } else {
            assert_eq!(action, InputAction::Consumed);
        }
    }

    #[tokio::test]
    async fn globals_own_keys_fit_and_restore_the_exact_palette_and_base() {
        tokio::time::timeout(Duration::from_secs(10), async {
            for surface in [
                Surface::ModelSelector,
                Surface::ProfileSwitcher,
                Surface::UsagePanel,
                Surface::Login,
            ] {
                for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                    for full in [false, true] {
                        for base in [FocusState::Input, FocusState::Chat] {
                            let mut state = TuiState::new(width, height);
                            state.focus = base.clone();
                            state.input_buffer = if full {
                                ["draft"; 8].join("\n")
                            } else {
                                String::new()
                            };
                            let original_layout = k3_compose::layout(&state);
                            press(&mut state, Key::Ctrl('p'));
                            press(&mut state, Key::Char('x'));
                            let caller = state.modal_stack.top().unwrap().clone();
                            let query = state.command_palette.filter_text.clone();
                            open(&mut state, surface).await;
                            let top = state.modal_stack.top().unwrap().clone();
                            assert!(top.id > caller.id);
                            assert_eq!(state.modal_stack.len(), 2);
                            press(&mut state, Key::Char('y'));
                            press(&mut state, Key::Down);
                            assert_eq!(
                                state.command_palette.filter_text, query,
                                "only the top receives keys, including its caret"
                            );
                            match surface {
                                Surface::ModelSelector => {
                                    assert_eq!(state.model_selector.selected_model, 1)
                                }
                                Surface::ProfileSwitcher => {
                                    assert_eq!(state.profile_switcher.selected, 1)
                                }
                                Surface::UsagePanel => {
                                    assert_eq!(state.usage_panel.selected_section, 1)
                                }
                                Surface::Login => assert_eq!(state.login.query, "y"),
                                _ => unreachable!(),
                            }
                            let painted = k3_compose::draw(&state, &original_layout, None);
                            let rect = presentation::geometry::frame_rect(
                                presentation::geometry::modal_region(
                                    &original_layout,
                                    painted.area,
                                ),
                            );
                            assert_eq!(painted[(rect.x, rect.y)].symbol(), "┌");
                            assert_eq!(
                                painted[(rect.right() - 1, rect.bottom() - 1)].symbol(),
                                "┘"
                            );
                            assert!(
                                (rect.y + 1..rect.bottom() - 1).any(|y| !k3_compose::row(
                                    &painted, rect, y
                                )
                                .trim_matches(['│', ' '])
                                .is_empty()),
                                "{surface:?}: body at {width}x{height}, full={full}"
                            );
                            assert!(
                                k3_compose::row(&painted, rect, rect.y).contains(surface.title())
                            );
                            let footer = if surface == Surface::ProfileSwitcher {
                                "Unable to compute diff"
                            } else {
                                "Esc"
                            };
                            assert!(
                                (rect.y + 1..rect.bottom())
                                    .any(|y| k3_compose::row(&painted, rect, y).contains(footer)),
                                "{surface:?}: existing footer remains inside the sheet"
                            );
                            k3_compose::open_quit(&mut state);
                            press(&mut state, Key::Char('n'));
                            assert_eq!(state.modal_stack.top(), Some(&top));
                            back(&mut state);
                            if surface == Surface::Login {
                                assert_eq!(
                                    state.modal_stack.top(),
                                    Some(&top),
                                    "first Esc clears Login's query"
                                );
                                assert!(state.login.query.is_empty());
                                back(&mut state);
                            }
                            assert_eq!(
                                state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                                vec![caller]
                            );
                            assert_eq!(state.command_palette.filter_text, query);
                            assert_eq!(state.focus, base);
                            back(&mut state);
                            assert!(state.modal_stack.is_empty());
                            assert_eq!(state.focus, base);
                        }
                    }
                }
            }
        })
        .await
        .expect("bounded global surface matrix");
    }

    #[tokio::test]
    async fn migrated_palette_targets_handoff_to_the_palettes_caller() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut actions = vec![
                PaletteAction::SwitchModel {
                    provider_id: "fixture".into(),
                    model_id: "two".into(),
                },
                PaletteAction::SwitchProfile("two".into()),
            ];
            actions.extend(
                ["models", "login", "logout", "accounts"]
                    .map(|name| PaletteAction::ExecuteCommand(name.into(), None)),
            );
            for action in actions {
                for nested in [false, true] {
                    let mut state = TuiState::new(80, 24);
                    state.focus = FocusState::Chat;
                    state.input_buffer = "exact caller draft".into();
                    if nested {
                        press(&mut state, Key::Ctrl('x'));
                        press(&mut state, Key::Char('?'));
                        state.help_overlay.scroll_offset = 7;
                    }
                    let caller = state.modal_stack.iter().cloned().collect::<Vec<_>>();
                    press(&mut state, Key::Ctrl('p'));
                    state.command_palette.filtered_entries.push(PaletteEntry {
                        name: "audited destination".into(),
                        description: String::new(),
                        shortcut: None,
                        scope: PaletteScope::All,
                        action: action.clone(),
                    });
                    let effect = press(&mut state, Key::Enter);
                    match effect {
                        InputAction::SwitchModelProvider {
                            provider_id,
                            model_id,
                        } => {
                            let (tx, _) = tokio::sync::mpsc::unbounded_channel();
                            let conversation = crate::e2e_harness::TestHarness::new().conversation;
                            model_switch::handle_apply_model_switch(
                                &mut state,
                                &Info,
                                &StreamingState::default(),
                                &tx,
                                &conversation,
                                provider_id,
                                model_id,
                            )
                            .await;
                            assert_eq!(state.selected_model.as_deref(), Some("two"));
                        }
                        InputAction::ProfileSwitchRequested(name) => {
                            profile_switch::open(&mut state, |selector| {
                                selector.initialize(profiles(), "one", Default::default());
                            });
                            profile_switch::preview(&mut state, Some(&name));
                            assert_eq!(state.profile_switcher.selected, 1);
                        }
                        InputAction::ExecuteCommand { name, args } => match name.as_str() {
                            "models" => model_switch::open_selector(
                                &mut state,
                                &Info,
                                &AppConfig::default(),
                                None,
                            ),
                            "login" => {
                                login::command(&mut state, &AppConfig::default(), args.as_deref());
                            }
                            "logout" | "accounts" => {
                                accounts::command(&mut state, &name, args.as_deref());
                            }
                            _ => panic!("unexpected command"),
                        },
                        other => panic!("audited palette action was refused: {other:?}"),
                    }
                    assert!(
                        !presentation::is_open(&state, Surface::CommandPalette),
                        "{action:?}"
                    );
                    let destination = match &action {
                        PaletteAction::SwitchModel { .. } => None,
                        PaletteAction::SwitchProfile(_) => Some(Surface::ProfileSwitcher),
                        PaletteAction::ExecuteCommand(name, _) if name == "models" => {
                            Some(Surface::ModelSelector)
                        }
                        _ => Some(Surface::Login),
                    };
                    if let Some(destination) = destination {
                        assert_eq!(state.modal_stack.top().unwrap().surface, destination);
                        back(&mut state);
                        if presentation::is_open(&state, destination) {
                            back(&mut state);
                        }
                    }
                    assert_eq!(
                        state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                        caller,
                        "{action:?}"
                    );
                    assert_eq!(state.focus, FocusState::Chat);
                    assert_eq!(state.input_buffer, "exact caller draft");
                    assert!(
                        state.notifications.records().all(|r| r.message
                            != rustain::adapters::tui::keymap::table::CLOSE_WINDOW_FIRST)
                    );
                    if nested {
                        assert_eq!(state.help_overlay.scroll_offset, 7);
                    }
                }
            }
        })
        .await
        .expect("bounded palette handoff matrix");
    }

    #[tokio::test]
    async fn depth_four_drilldown_replaces_utility_and_decision_restore() {
        use rustain::adapters::tui::{
            decision_queue::{self, Arrival, DecisionDraft, DecisionKind},
            notification_log::LogFacts,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            for fifth in [Surface::UsagePanel, Surface::Help, Surface::Decision] {
                let mut state = TuiState::new(80, 24);
                let clock = Arc::new(rustain::domain::clock::MockClock::new(
                    std::time::Instant::now(),
                ));
                state.notifications =
                    rustain::adapters::tui::notifications::NotificationStore::from_config(
                        &rustain::domain::models::NotificationsConfig::default(),
                        clock.clone(),
                    );
                state.focus = FocusState::Chat;
                state.input_buffer = "untouched".into();
                press(&mut state, Key::Ctrl('p'));
                open(&mut state, Surface::ModelSelector).await;
                open(&mut state, Surface::ProfileSwitcher).await;
                if fifth == Surface::UsagePanel {
                    press(&mut state, Key::Ctrl('x'));
                    press(&mut state, Key::Char('n'));
                } else {
                    open(&mut state, Surface::UsagePanel).await;
                }
                assert_eq!(
                    state.modal_stack.len(),
                    4,
                    "all four distinct L2 openers reached"
                );
                let before = state.modal_stack.iter().cloned().collect::<Vec<_>>();
                if fifth == Surface::UsagePanel {
                    open(&mut state, fifth).await;
                } else if fifth == Surface::Help {
                    press(&mut state, Key::Ctrl('x'));
                    press(&mut state, Key::Char('?'));
                } else {
                    decision_queue::raise(
                        &mut state,
                        DecisionDraft::new(DecisionKind::Budget, "queued at cap"),
                        Arrival::Automatic,
                    );
                    assert_eq!(
                        state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                        before,
                        "arrival waits under draft"
                    );
                    press(&mut state, Key::Ctrl('x'));
                    assert_eq!(
                        press(&mut state, Key::Char('q')),
                        InputAction::FocusDecisions
                    );
                    decision_queue::open_waiting(&mut state, &LogFacts::default());
                    assert_eq!(press(&mut state, Key::Enter), InputAction::Consumed);
                }
                assert_eq!(state.modal_stack.len(), 4);
                assert_eq!(state.modal_stack.top().unwrap().surface, fifth);
                assert!(state.modal_stack.top().unwrap().id > before[3].id);
                assert!(presentation::consistent(&state));
                if fifth == Surface::Decision {
                    k3_compose::draw(&state, &k3_compose::layout(&state), None);
                    decision_queue::note_painted(&mut state);
                    clock.advance(decision_queue::INERT_AFTER_PAINT);
                }
                back(&mut state);
                if fifth == Surface::UsagePanel {
                    assert_eq!(
                        state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                        before[..3]
                    );
                    assert!(!presentation::is_open(&state, Surface::NotificationLog));
                } else {
                    assert_eq!(
                        state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                        before
                    );
                    back(&mut state);
                }
                for remaining in (0..3).rev() {
                    back(&mut state);
                    assert_eq!(state.modal_stack.len(), remaining);
                }
                assert_eq!(state.input_buffer, "untouched");
                assert_eq!(state.focus, FocusState::Chat);
            }
        })
        .await
        .expect("bounded reachable depth-cap cases");
    }

    #[tokio::test]
    async fn locked_model_enter_hands_off_and_cancel_returns_to_the_picker_caller() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut state = TuiState::new(80, 24);
            state.focus = FocusState::Chat;
            press(&mut state, Key::Ctrl('p'));
            let caller = state.modal_stack.top().cloned();
            open(&mut state, Surface::ModelSelector).await;
            press(&mut state, Key::Right);
            assert_eq!(
                state.model_selector.columns[state.model_selector.selected_provider].provider_id,
                "anthropic"
            );
            let model = state.modal_stack.top().unwrap().id;
            let InputAction::Login(action) = press(&mut state, Key::Enter) else {
                panic!("locked Enter routes Login")
            };
            login_action(&mut state, action);
            assert_eq!(
                state.modal_stack.len(),
                2,
                "handoff does not add an Esc level"
            );
            assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Login);
            assert!(!presentation::is_open(&state, Surface::ModelSelector));
            assert_eq!(
                state.login.view,
                rustain::adapters::tui::state::LoginView::MethodPicker
            );
            back(&mut state);
            assert_eq!(
                state.modal_stack.top().unwrap().surface,
                Surface::ModelSelector
            );
            assert!(state.modal_stack.top().unwrap().id > model);
            assert_eq!(
                state.model_selector.columns[state.model_selector.selected_provider].provider_id,
                "anthropic"
            );
            back(&mut state);
            assert_eq!(state.modal_stack.top(), caller.as_ref());
            assert_eq!(state.focus, FocusState::Chat);
        })
        .await
        .expect("bounded locked model handoff");
    }

    #[tokio::test]
    async fn ba_smoke_predicates_have_real_rust_frames() {
        use rustain::adapters::{command_registry, palette_registry};
        fn emit(h: &mut crate::e2e_harness::TestHarness, stage: &str) {
            let width = usize::from(h.state.terminal_width);
            let height = h.state.terminal_height;
            h.render();
            let buffer = h.terminal.backend().buffer();
            let rows = (0..height)
                .map(|y| k3_compose::row(buffer, buffer.area, y))
                .collect::<Vec<_>>();
            assert!(rows.iter().all(|row| row.chars().count() == width));
            println!(
                "BA_DESKCHECK_FRAME={}",
                serde_json::json!([width, height, stage, rows.join("\n")])
            );
        }
        let mut registry = palette_registry::PaletteRegistry::new();
        registry.populate_from_command_registry(&command_registry::CommandRegistry::new());
        tokio::time::timeout(Duration::from_secs(5), async {
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                let mut h = crate::e2e_harness::TestHarness::with_size(width, height);
                h.state.input_buffer = "A1-kept-draft".into();
                open(&mut h.state, Surface::ModelSelector).await;
                emit(&mut h, "model-alone");
                back(&mut h.state);
                press(&mut h.state, Key::Ctrl('x'));
                press(&mut h.state, Key::Char('?'));
                for _ in 0..3 {
                    press(&mut h.state, Key::Char('j'));
                }
                let caller = h.state.modal_stack.top().cloned();
                emit(&mut h, "help-before");
                press(&mut h.state, Key::Ctrl('p'));
                let entries = registry.all_entries().into_iter().cloned().collect();
                h.state.command_palette.filtered_entries = entries;
                emit(&mut h, "palette-all");
                press(&mut h.state, Key::Char('~'));
                h.state.command_palette.filtered_entries.clear();
                emit(&mut h, "palette-empty");
                press(&mut h.state, Key::Ctrl('u'));
                for c in "/models".chars() {
                    press(&mut h.state, Key::Char(c));
                }
                h.state.command_palette.filtered_entries = registry
                    .fuzzy_filter("models", Some(PaletteScope::SlashCommand))
                    .into_iter()
                    .cloned()
                    .collect();
                emit(&mut h, "palette-models");
                assert_eq!(
                    press(&mut h.state, Key::Enter),
                    InputAction::ExecuteCommand {
                        name: "models".into(),
                        args: None
                    }
                );
                model_switch::open_selector(&mut h.state, &Info, &AppConfig::default(), None);
                assert!(!presentation::is_open(&h.state, Surface::CommandPalette));
                emit(&mut h, "help-model");
                back(&mut h.state);
                assert_eq!(h.state.modal_stack.top(), caller.as_ref());
                emit(&mut h, "help-return");
                back(&mut h.state);
                emit(&mut h, "base-return");
            }
        })
        .await
        .expect("bounded real Ba predicate frames");
    }

    #[tokio::test]
    async fn covered_login_reopens_an_existing_lower_model_without_losing_quit() {
        use rustain::adapters::tui::{
            handlers::{AuthWork, HandlerOutcome, SpawnRequest},
            state::RunningLogin,
        };
        use rustain::domain::events::{LoginEvent, LoginSummary};
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut state = TuiState::new(80, 24);
            state.focus = FocusState::Chat;
            open(&mut state, Surface::ModelSelector).await;
            press(&mut state, Key::Down);
            let selected = state.model_selector.selected_model;
            assert_eq!(selected, 1, "a changed selection is retained on reopen");
            let model = state.modal_stack.top().cloned().unwrap();
            open(&mut state, Surface::ProfileSwitcher).await;
            press(&mut state, Key::Ctrl('p'));
            state.command_palette.filtered_entries.push(PaletteEntry {
                name: "/login deepseek".into(),
                description: String::new(),
                shortcut: None,
                scope: PaletteScope::All,
                action: PaletteAction::ExecuteCommand("login".into(), Some("deepseek".into())),
            });
            assert_eq!(
                press(&mut state, Key::Enter),
                InputAction::ExecuteCommand {
                    name: "login".into(),
                    args: Some("deepseek".into()),
                }
            );
            let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Login {
                login_id,
                origin,
                ..
            })) = login::command(&mut state, &AppConfig::default(), Some("deepseek"))
            else {
                panic!("real single-method login spawn effect");
            };
            state.login.start_running(RunningLogin {
                login_id,
                generation: state.login.generation,
                origin,
                provider_id: "deepseek".into(),
                display: "DeepSeek".into(),
                cancel: tokio_util::sync::CancellationToken::new(),
                sections: vec![],
                progress: vec![],
                field: None,
            });
            k3_compose::open_quit(&mut state);
            let quit = state.modal_stack.top().cloned().unwrap();
            login::on_event(
                &mut state,
                &AppConfig::default(),
                &Info,
                LoginEvent::Finished {
                    login_id,
                    provider: "deepseek".into(),
                    outcome: Ok(LoginSummary {
                        lines: vec!["Login completed".into()],
                        credential_id: rustain::domain::models::CredentialId(1),
                        notes: Vec::new(),
                    }),
                },
            );
            assert_eq!(
                state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                vec![model.clone(), quit]
            );
            assert_eq!(state.focus, FocusState::Chat);
            press(&mut state, Key::Char('n'));
            assert_eq!(state.modal_stack.top(), Some(&model));
            assert_eq!(state.model_selector.selected_model, selected);
            k3_compose::draw(&state, &k3_compose::layout(&state), Some("Select Model"));
            back(&mut state);
            assert!(state.modal_stack.is_empty());
        })
        .await
        .expect("bounded covered completion");
    }

    #[tokio::test]
    async fn arrivals_wait_for_model_and_login_but_preempt_profile_and_usage() {
        use rustain::adapters::tui::decision_queue::Raised;
        tokio::time::timeout(Duration::from_secs(5), async {
            for surface in [
                Surface::ModelSelector,
                Surface::ProfileSwitcher,
                Surface::UsagePanel,
                Surface::Login,
            ] {
                let (mut state, clock) = k5_restore::state();
                open(&mut state, surface).await;
                clock.advance(Duration::from_secs(2));
                let caller = state.modal_stack.top().cloned().unwrap();
                let protected = matches!(surface, Surface::ModelSelector | Surface::Login);
                let expected = if protected {
                    Raised::Waiting
                } else {
                    Raised::Focused
                };
                assert_eq!(k5_restore::arrival(&mut state), expected, "{surface:?}");
                if protected {
                    assert_eq!(state.modal_stack.top(), Some(&caller));
                    assert!(state.notifications.records().any(|record| record.message
                        == rustain::adapters::tui::decision_queue::waiting_toast()));
                } else {
                    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Decision);
                    k5_restore::ready(&mut state, &clock);
                    back(&mut state);
                    assert_eq!(state.modal_stack.top(), Some(&caller));
                }
                k3_compose::draw(&state, &k3_compose::layout(&state), Some(surface.title()));
                assert_eq!(state.focus, FocusState::Input);
                assert!(state.input_buffer.is_empty());
            }
        })
        .await
        .expect("bounded independent global arrival cases");
    }

    #[test]
    fn bookmarks_open_through_the_real_handler_as_a_visible_frame() {
        use rustain::adapters::tui::handlers::bookmark;
        let (mut h, tabs) = super::stage_support::stage(80, 24);
        let before = super::stage_support::view(&tabs);
        assert_eq!(
            press(&mut h.state, Key::Char('\'')),
            InputAction::OpenBookmarkList
        );
        bookmark::handle_apply_open_bookmark_list(
            &tabs,
            &mut h.state,
            &tabs.active_tab().conversation,
        );
        assert_eq!(
            h.state.modal_stack.top().map(|f| f.surface.title()),
            Some("Bookmarks")
        );
        h.render_active_tab(&tabs);
        assert!(
            h.screen_contains("Bookmarks (2)"),
            "actual tab bookmark facts must paint"
        );
        assert!(h.screen_contains("Jump"));
        press(&mut h.state, Key::Esc);
        assert!(h.state.modal_stack.is_empty());
        assert_eq!(super::stage_support::view(&tabs), before);
    }

    #[test]
    fn bookmark_acceptance_moves_the_authoritative_tab_not_only_the_mirrors() {
        use rustain::adapters::tui::handlers::bookmark;
        let (mut h, mut tabs) = super::stage_support::stage(80, 24);
        bookmark::handle_apply_open_bookmark_list(
            &tabs,
            &mut h.state,
            &tabs.active_tab().conversation,
        );
        let before = super::stage_support::view(&tabs);
        h.state.bookmark_list_selected = 0;
        let conversation = tabs.active_tab().conversation.clone();
        bookmark::handle_apply_jump_bookmark(&mut tabs, &mut h.state, &conversation);
        assert_ne!(
            super::stage_support::view(&tabs),
            before,
            "observable move in the same manager"
        );
        assert_eq!(
            tabs.active_tab().view_state.scroll_offset,
            h.state.scroll_offset()
        );
        assert!(h.state.modal_stack.is_empty());
    }

    #[test]
    fn retained_wave_opener_owns_a_frame_without_overwriting_base_focus() {
        use rustain::domain::models::FocusState;
        use std::sync::Arc;
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Chat;
        state.wave_run = Some(Arc::new(super::keymap_support::StubWave(3)));
        assert_eq!(
            press(&mut state, Key::Char('r')),
            InputAction::OpenWaveOverlay
        );
        assert_eq!(
            state.modal_stack.top().map(|f| f.surface.title()),
            Some("Wave")
        );
        assert_eq!(state.focus, FocusState::Chat);
    }

    fn stage_frame_for_pushes(
        wave: bool,
        diverge: bool,
    ) -> (
        crate::e2e_harness::TestHarness,
        rustain::domain::models::TabManager,
    ) {
        use rustain::adapters::tui::handlers::{bookmark, wave_overlay};
        let (mut h, tabs) = super::stage_support::stage(80, 24);
        if wave {
            h.state.wave_run = Some(std::sync::Arc::new(super::keymap_support::StubWave(3)));
            let action = press(&mut h.state, Key::Char('r'));
            wave_overlay::apply(&mut h.state, &tabs.active_tab().conversation.id, &action);
            if diverge {
                let action = press(&mut h.state, Key::Char('d'));
                wave_overlay::apply(&mut h.state, &tabs.active_tab().conversation.id, &action);
            }
        } else {
            bookmark::handle_apply_open_bookmark_list(
                &tabs,
                &mut h.state,
                &tabs.active_tab().conversation,
            );
        }
        (h, tabs)
    }

    fn assert_palette_push(
        h: &mut crate::e2e_harness::TestHarness,
        tabs: &rustain::domain::models::TabManager,
    ) {
        let owner = h.state.modal_stack.top().cloned().unwrap();
        let view = super::stage_support::view(tabs);
        let base = h.state.focus.clone();
        assert_eq!(press(&mut h.state, Key::Ctrl('p')), InputAction::Consumed);
        assert_eq!(
            h.state.modal_stack.top().unwrap().surface,
            Surface::CommandPalette
        );
        press(&mut h.state, Key::Esc);
        assert_eq!(h.state.modal_stack.top(), Some(&owner));
        assert_eq!(h.state.focus, base);
        assert_eq!(super::stage_support::view(tabs), view);
        assert_eq!(
            press(&mut h.state, Key::Ctrl('q')),
            InputAction::Quit,
            "unconditional precedence"
        );
    }

    #[test]
    fn bookmark_wave_palette_pushes_restore_the_actual_caller() {
        for wave in [false, true] {
            let (mut h, tabs) = stage_frame_for_pushes(wave, false);
            assert_palette_push(&mut h, &tabs);
        }
    }

    #[test]
    fn wave_divergence_palette_push_restores_the_chat_wave_subcontext() {
        let (mut h, tabs) = stage_frame_for_pushes(true, true);
        assert_palette_push(&mut h, &tabs);
        assert!(h.state.wave_diverge_open);
        assert_eq!(
            rustain::adapters::tui::presentation::frame_context(&h.state, Surface::WaveOverlay),
            rustain::adapters::tui::keymap::Context::ChatWave
        );
    }
}

mod k8_stage {
    use super::{k3_compose, k7_surfaces, keymap_support::press, stage_support};
    use rustain::adapters::tui::{
        keymap::Key,
        presentation::Surface,
        state::{CachedTurnLayout, HeightKey, MessageHeightKey, TabRenderState},
    };
    use rustain::domain::models::{AnchorMode, StreamChunk, SummaryTier, TabManager, TurnId};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    pub(super) fn search_key(
        h: &mut crate::e2e_harness::TestHarness,
        tabs: &mut TabManager,
        key: Key,
    ) -> rustain::adapters::tui::app::InputAction {
        let action = press(&mut h.state, key);
        let conversation = tabs.active_tab().conversation.clone();
        rustain::adapters::tui::handlers::search::handle_search_action(
            tabs,
            &mut h.state,
            &conversation,
            &action,
        );
        action
    }

    fn searchable_tab() -> (crate::e2e_harness::TestHarness, TabManager) {
        let mut h = crate::e2e_harness::TestHarness::new();
        h.send_message(
            &(0..60)
                .map(|row| format!("search-row-{row:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut tabs = TabManager::new(CancellationToken::new());
        tabs.active_tab_mut().conversation = std::mem::take(&mut h.conversation);
        h.state.active_tab_id = tabs.active_tab().id;
        h.render_active_tab(&tabs);
        (h, tabs)
    }

    #[test]
    fn search_esc_restores_the_pre_search_view_exactly() {
        use rustain::domain::models::AnchorRef;
        for (mode, key) in [
            (AnchorMode::Following, Key::Esc),
            (AnchorMode::Reading, Key::Ctrl('c')),
            (
                AnchorMode::Pinned(AnchorRef {
                    turn_id: TurnId("focused-before-search".into()),
                    line_in_turn: 2,
                }),
                Key::Esc,
            ),
        ] {
            let (mut h, mut tabs) = searchable_tab();
            let view = &mut tabs.active_tab_mut().view_state;
            view.mode = mode;
            let offset = if matches!(view.mode, AnchorMode::Following) {
                0
            } else {
                3
            };
            view.scroll_offset = offset;
            view.focused_turn = Some(TurnId("focused-before-search".into()));
            view.summary_tier = SummaryTier::Tier2;
            h.state.set_scroll_offset(offset);
            h.state.set_auto_scroll(matches!(
                tabs.active_tab().view_state.mode,
                AnchorMode::Following
            ));
            h.render_active_tab(&tabs);
            let before = format!("{:?}", tabs.active_tab().view_state);
            let pixels = h.screen_text();
            let layout = k3_compose::layout(&h.state);
            let composer = (
                h.state.input_buffer.clone(),
                h.state.cursor_position,
                h.state.input_scroll_offset,
            );
            let cache = &h.state.tab_render_states[&tabs.active_tab().id];
            let mut cache_keys = cache
                .height_cache
                .entries
                .iter()
                .map(|(key, entry)| format!("{key:?}={entry:?}"))
                .collect::<Vec<_>>();
            cache_keys.sort();
            let mut message_keys = cache
                .height_cache
                .message_entries
                .iter()
                .map(|(key, height)| format!("{key:?}={height}"))
                .collect::<Vec<_>>();
            message_keys.sort();
            assert!(
                !cache_keys.is_empty() || !message_keys.is_empty(),
                "cache invariance has a populated production control"
            );
            search_key(&mut h, &mut tabs, Key::Ctrl('f'));
            assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::Search);
            assert_eq!(
                format!("{:?}", tabs.active_tab().view_state),
                before,
                "opening itself never navigates"
            );
            for c in "search-row-00".chars() {
                search_key(&mut h, &mut tabs, Key::Char(c));
            }
            assert!(
                h.state.scroll_offset() > 3,
                "fixture navigates away from the initial rows"
            );
            assert_eq!(
                tabs.active_tab().view_state.scroll_offset,
                h.state.scroll_offset()
            );
            assert_ne!(format!("{:?}", tabs.active_tab().view_state), before);
            h.render_active_tab(&tabs);
            assert_ne!(
                h.screen_text(),
                pixels,
                "the actual owning view reaches production painting"
            );
            assert_eq!(k3_compose::layout(&h.state).chat_pane, layout.chat_pane);
            assert_eq!(k3_compose::layout(&h.state).input_area, layout.input_area);
            search_key(&mut h, &mut tabs, key);
            assert!(!rustain::adapters::tui::presentation::is_open(
                &h.state,
                Surface::Search
            ));
            assert_eq!(format!("{:?}", tabs.active_tab().view_state), before);
            h.render_active_tab(&tabs);
            assert_eq!(
                h.screen_text(),
                pixels,
                "Back restores exact pre-search rows"
            );
            assert_eq!(
                (
                    h.state.input_buffer.clone(),
                    h.state.cursor_position,
                    h.state.input_scroll_offset
                ),
                composer
            );
            let cache = &h.state.tab_render_states[&tabs.active_tab().id];
            let mut after_keys = cache
                .height_cache
                .entries
                .iter()
                .map(|(key, entry)| format!("{key:?}={entry:?}"))
                .collect::<Vec<_>>();
            after_keys.sort();
            assert_eq!(after_keys, cache_keys);
            let mut after_messages = cache
                .height_cache
                .message_entries
                .iter()
                .map(|(key, height)| format!("{key:?}={height}"))
                .collect::<Vec<_>>();
            after_messages.sort();
            assert_eq!(after_messages, message_keys);
        }
    }

    #[test]
    fn navigating_enter_keeps_the_actual_match_view_and_back_disposal_is_exact() {
        let (mut h, mut tabs) = searchable_tab();
        let pixels = h.screen_text();
        search_key(&mut h, &mut tabs, Key::Ctrl('f'));
        for c in "search-row-00".chars() {
            search_key(&mut h, &mut tabs, Key::Char(c));
        }
        search_key(&mut h, &mut tabs, Key::Enter);
        let matched = format!("{:?}", tabs.active_tab().view_state);
        assert!(h.state.scroll_offset() > 0);
        search_key(&mut h, &mut tabs, Key::Enter);
        assert!(h.state.modal_stack.is_empty());
        assert_eq!(format!("{:?}", tabs.active_tab().view_state), matched);
        h.render_active_tab(&tabs);
        assert_ne!(
            h.screen_text(),
            pixels,
            "Keep position is observably different from Back"
        );
        search_key(&mut h, &mut tabs, Key::Ctrl('f'));
        search_key(&mut h, &mut tabs, Key::Esc);
        assert_eq!(
            format!("{:?}", tabs.active_tab().view_state),
            matched,
            "a new opening snapshots the kept position, not retired state"
        );
    }

    #[test]
    fn search_stage_wheel_navigates_but_a_covering_frame_consumes_it() {
        use rustain::adapters::tui::{app, handlers::scroll};
        use rustain::domain::{events::DomainInputEvent, models::view_state::ScrollDelta};
        let (mut h, mut tabs) = searchable_tab();
        let before = format!("{:?}", tabs.active_tab().view_state);
        search_key(&mut h, &mut tabs, Key::Ctrl('f'));
        let wheel = DomainInputEvent::MouseScroll(ScrollDelta::WheelUp(5));
        let app::InputAction::MouseScroll(delta) = app::handle_input(&mut h.state, &wheel) else {
            panic!("Search uses its Stage body for explicit navigation");
        };
        let conversation = tabs.active_tab().conversation.clone();
        scroll::handle_apply_scroll_intent(&mut tabs, &mut h.state, &conversation, delta);
        assert_ne!(format!("{:?}", tabs.active_tab().view_state), before);
        search_key(&mut h, &mut tabs, Key::Ctrl('x'));
        search_key(&mut h, &mut tabs, Key::Char('?'));
        assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::Help);
        let covered = format!("{:?}", tabs.active_tab().view_state);
        assert_eq!(
            app::handle_input(&mut h.state, &wheel),
            app::InputAction::Consumed
        );
        assert_eq!(format!("{:?}", tabs.active_tab().view_state), covered);
        search_key(&mut h, &mut tabs, Key::Esc);
        assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::Search);
        search_key(&mut h, &mut tabs, Key::Esc);
        assert_eq!(format!("{:?}", tabs.active_tab().view_state), before);
    }

    #[test]
    fn navigating_enter_requests_keep_position() {
        use rustain::adapters::tui::{app, state::SearchSubstate};
        let mut state = rustain::adapters::tui::state::TuiState::new(80, 24);
        press(&mut state, Key::Ctrl('f'));
        state.search_state.substate = SearchSubstate::Navigating;
        assert_eq!(
            press(&mut state, Key::Enter),
            app::InputAction::SearchCommit
        );
    }

    #[test]
    fn active_tab_scroll_control_changes_the_rendered_view() {
        use rustain::adapters::tui::{app, handlers::scroll};
        use rustain::domain::{events::DomainInputEvent, models::view_state::ScrollDelta};

        let mut h = crate::e2e_harness::TestHarness::new();
        h.send_message(
            &(0..60)
                .map(|row| format!("active-row-{row:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut tabs = TabManager::new(CancellationToken::new());
        tabs.active_tab_mut().conversation = std::mem::take(&mut h.conversation);
        h.state.active_tab_id = tabs.active_tab().id;
        h.render_active_tab(&tabs);
        h.assert_screen_contains("active-row-59", "the owning active tab is rendered");
        let before = h.screen_text();
        let view = format!("{:?}", tabs.active_tab().view_state);
        let action = app::handle_input(
            &mut h.state,
            &DomainInputEvent::MouseScroll(ScrollDelta::WheelUp(30)),
        );
        let app::InputAction::MouseScroll(delta) = action else {
            panic!("production dispatch must emit the scroll effect");
        };
        let conversation = tabs.active_tab().conversation.clone();
        scroll::handle_apply_scroll_intent(&mut tabs, &mut h.state, &conversation, delta);
        assert_ne!(format!("{:?}", tabs.active_tab().view_state), view);
        h.render_active_tab(&tabs);
        assert_ne!(
            h.screen_text(),
            before,
            "active view mutation must reach paint"
        );
        h.assert_screen_not_contains("active-row-59", "scroll changes the visible rows");
    }

    #[tokio::test]
    async fn globals_preserve_the_tab_view_cache_keys_and_composer() {
        tokio::time::timeout(Duration::from_secs(5), async {
            for surface in [
                Surface::ModelSelector,
                Surface::ProfileSwitcher,
                Surface::UsagePanel,
                Surface::Login,
            ] {
                let mut h = crate::e2e_harness::TestHarness::new();
                let state = &mut h.state;
                state.input_buffer = "first λ\nsecond".into();
                state.cursor_position = 3;
                state.input_scroll_offset = 1;
                let mut tabs = TabManager::new(CancellationToken::new());
                let tab = tabs.active_tab_mut();
                state.active_tab_id = tab.id;
                let mut cached_turn = rustain::domain::models::Turn::new(String::new(), 0);
                cached_turn.id = TurnId("sentinel".into());
                tab.conversation.turns.push(cached_turn);
                tab.feedback_blocks.insert(
                    "stage".into(),
                    rustain::domain::models::FeedbackBlock {
                        id: "stage".into(),
                        level: rustain::domain::models::FeedbackLevel::Info,
                        message: "stage row\n".repeat(60),
                        seq: 1,
                    },
                );
                tab.view_state.mode = AnchorMode::Reading;
                tab.view_state.scroll_offset = 9;
                tab.view_state
                    .collapsed
                    .insert(TurnId("sentinel".into()), true);
                tab.view_state.focused_turn = Some(TurnId("sentinel".into()));
                tab.view_state.pending_append_lines = 4;
                let view = format!("{:?}", tab.view_state);
                let key = HeightKey {
                    turn_id: TurnId("sentinel".into()),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                };
                let message = MessageHeightKey {
                    msg_id: "message".into(),
                    terminal_width: 80,
                    content_hash: 13,
                };
                let mut render = TabRenderState {
                    cached_width: Some(80),
                    ..Default::default()
                };
                render.height_cache.entries.insert(
                    key.clone(),
                    CachedTurnLayout {
                        height: 4,
                        block_offsets: vec![0],
                        tool_block_offsets: vec![],
                        md_block_offsets: vec![],
                        reasoning_offsets: vec![],
                    },
                );
                render
                    .height_cache
                    .message_entries
                    .insert(message.clone(), 2);
                state.tab_render_states.insert(tab.id, render);
                let layout = k3_compose::layout(state);
                let composer = (
                    state.input_buffer.clone(),
                    state.cursor_position,
                    state.input_scroll_offset,
                );
                h.render_active_tab(&tabs);
                let cache_snapshot = stage_support::cache_keys(&h, &tabs);
                assert_eq!(format!("{:?}", tabs.active_tab().view_state), view);
                let stage = h.screen_text();
                k7_surfaces::open(&mut h.state, surface).await;
                for during in [true, false] {
                    if !during {
                        k7_surfaces::back(&mut h.state);
                    } else {
                        press(&mut h.state, Key::Down);
                        if surface == Surface::Login {
                            let frame = h.state.modal_stack.top().cloned();
                            assert_eq!(
                                press(&mut h.state, Key::ShiftTab),
                                rustain::adapters::tui::app::InputAction::Consumed
                            );
                            assert_eq!(h.state.modal_stack.top(), frame.as_ref());
                        }
                    }
                    h.render_active_tab(&tabs);
                    let state = &h.state;
                    assert_eq!(k3_compose::layout(state).chat_pane, layout.chat_pane);
                    assert_eq!(k3_compose::layout(state).input_area, layout.input_area);
                    assert_eq!(format!("{:?}", tabs.active_tab().view_state), view);
                    assert_eq!(
                        (
                            state.input_buffer.clone(),
                            state.cursor_position,
                            state.input_scroll_offset
                        ),
                        composer
                    );
                    let render = &state.tab_render_states[&tabs.active_tab().id];
                    assert_eq!(stage_support::cache_keys(&h, &tabs), cache_snapshot);
                    assert_eq!(render.cached_width, Some(80));
                    assert_eq!(
                        render.height_cache.entries.keys().collect::<Vec<_>>(),
                        vec![&key]
                    );
                    assert_eq!(
                        render
                            .height_cache
                            .message_entries
                            .keys()
                            .collect::<Vec<_>>(),
                        vec![&message]
                    );
                }
                assert!(h.state.modal_stack.is_empty());
                assert_eq!(
                    h.screen_text(),
                    stage,
                    "back restores the same Stage pixels"
                );
            }
        })
        .await
        .expect("bounded global invariance");
    }

    #[tokio::test]
    async fn a_real_stream_keeps_appending_under_every_global_sheet() {
        tokio::time::timeout(Duration::from_secs(5), async {
            for surface in [
                Surface::ModelSelector,
                Surface::ProfileSwitcher,
                Surface::UsagePanel,
                Surface::Login,
            ] {
                let mut h = crate::e2e_harness::TestHarness::new();
                h.send_message("stream beneath the sheet");
                h.process_chunk(StreamChunk::Text {
                    content: "first line\n".into(),
                    parent_tool_use_id: None,
                });
                h.render();
                assert!(h.streaming.is_streaming);
                k7_surfaces::open(&mut h.state, surface).await;
                let frame = h.state.modal_stack.top().cloned();
                h.process_chunk(StreamChunk::Text {
                    content: "independent appended row".into(),
                    parent_tool_use_id: None,
                });
                h.render();
                assert!(h.streaming.is_streaming);
                assert_eq!(h.state.modal_stack.top(), frame.as_ref());
                k7_surfaces::back(&mut h.state);
                h.render();
                assert!(h.state.modal_stack.is_empty());
                h.assert_screen_contains(
                    "independent appended row",
                    "real stream remains rendered after back",
                );
            }
        })
        .await
        .expect("bounded streaming invariance");
    }

    #[test]
    fn bookmark_and_wave_keep_the_real_view_cache_and_unreserved_shell() {
        use super::stage_support::{cache_keys, chat_pixels, stage, view};
        use rustain::adapters::tui::{
            app,
            handlers::{bookmark, scroll, wave_overlay},
        };
        use rustain::domain::{events::DomainInputEvent, models::view_state::ScrollDelta};
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            for full in [false, true] {
                for wave in [false, true] {
                    let (mut h, mut tabs) = stage(width, height);
                    if full {
                        h.state.input_buffer = "composer\n".repeat(12);
                    }
                    h.state.cursor_position = 3;
                    h.state.wave_run =
                        wave.then(|| std::sync::Arc::new(super::keymap_support::StubWave(3)) as _);
                    h.render_active_tab(&tabs);
                    let before_view = view(&tabs);
                    let before_keys = cache_keys(&h, &tabs);
                    assert!(
                        !before_keys.0.is_empty() || !before_keys.1.is_empty(),
                        "real render populates cache keys"
                    );
                    let before_pixels = chat_pixels(&h);
                    let layout = k3_compose::layout(&h.state);
                    let composer = (h.state.input_buffer.clone(), h.state.cursor_position);
                    let action = press(
                        &mut h.state,
                        if wave {
                            Key::Char('r')
                        } else {
                            Key::Char('\'')
                        },
                    );
                    if wave {
                        wave_overlay::apply(
                            &mut h.state,
                            &tabs.active_tab().conversation.id,
                            &action,
                        );
                    } else {
                        bookmark::handle_apply_open_bookmark_list(
                            &tabs,
                            &mut h.state,
                            &tabs.active_tab().conversation,
                        );
                    }
                    assert!(h.state.modal_stack.top().is_some());
                    let owner = h.state.modal_stack.top().cloned();
                    press(&mut h.state, Key::Char('j'));
                    let wheel = DomainInputEvent::MouseScroll(ScrollDelta::WheelUp(12));
                    assert_eq!(
                        app::handle_input(&mut h.state, &wheel),
                        app::InputAction::Consumed
                    );
                    for closing in [false, true] {
                        if closing {
                            let action = press(&mut h.state, Key::Esc);
                            if wave {
                                wave_overlay::apply(
                                    &mut h.state,
                                    &tabs.active_tab().conversation.id,
                                    &action,
                                );
                            }
                        }
                        h.render_active_tab(&tabs);
                        assert_eq!(k3_compose::layout(&h.state).chat_pane, layout.chat_pane);
                        assert_eq!(k3_compose::layout(&h.state).input_area, layout.input_area);
                        assert_eq!(view(&tabs), before_view);
                        assert_eq!(cache_keys(&h, &tabs), before_keys);
                        assert_eq!(
                            (h.state.input_buffer.clone(), h.state.cursor_position),
                            composer
                        );
                        if !closing {
                            assert_eq!(h.state.modal_stack.top(), owner.as_ref());
                            assert!(
                                h.screen_contains(if wave { "close" } else { "Jump" }),
                                "primary action fits {width}x{height}, full={full}"
                            );
                        }
                    }
                    assert!(h.state.modal_stack.is_empty());
                    assert_eq!(chat_pixels(&h), before_pixels);
                    let app::InputAction::MouseScroll(delta) =
                        app::handle_input(&mut h.state, &wheel)
                    else {
                        panic!("uncovered scroll control");
                    };
                    let conversation = tabs.active_tab().conversation.clone();
                    scroll::handle_apply_scroll_intent(
                        &mut tabs,
                        &mut h.state,
                        &conversation,
                        delta,
                    );
                    h.render_active_tab(&tabs);
                    assert_ne!(view(&tabs), before_view, "same manager actually moves");
                    assert_ne!(
                        chat_pixels(&h),
                        before_pixels,
                        "view mutation reaches real paint"
                    );
                }
            }
        }
    }
}

mod k7_reverse {
    use super::keymap_support::press;
    use rustain::adapters::tui::{app, keymap::Key, presentation::Surface, state::TuiState};
    use rustain::domain::models::FocusState;

    #[test]
    fn reverse_search_is_an_input_popup_and_never_a_frame() {
        let mut state = TuiState::new(80, 24);
        state.input_history.push("alpha one".into());
        state.input_history.push("beta".into());
        state.input_history.push("alpha two".into());
        state.input_buffer = "untouched draft".into();
        state.cursor_position = 3;
        press(&mut state, Key::Ctrl('r'));
        assert_eq!(state.focus, FocusState::Input);
        assert!(state.modal_stack.is_empty());
        assert!(state.reverse_search.is_open());
        app::handle_input(
            &mut state,
            &rustain::domain::events::DomainInputEvent::Paste("must not leak".into()),
        );
        assert_eq!(state.input_buffer, "untouched draft");
        assert_eq!(state.cursor_position, 3);
        assert_eq!(state.reverse_search.query.as_str(), "must not leak");
        press(&mut state, Key::Ctrl('u'));
        assert!(state.reverse_search.query.is_empty());
        for c in "alpha".chars() {
            assert_eq!(press(&mut state, Key::Char(c)), app::InputAction::Consumed);
        }
        assert_eq!(state.input_buffer, "untouched draft");
        press(&mut state, Key::Ctrl('r'));
        press(&mut state, Key::Enter);
        assert!(state.input_buffer.starts_with("alpha "));
        assert_eq!(state.focus, FocusState::Input);
        assert!(state.modal_stack.is_empty());
        let selected = state.input_buffer.clone();
        press(&mut state, Key::Ctrl('r'));
        press(&mut state, Key::Char('z'));
        press(&mut state, Key::Esc);
        assert_eq!(state.input_buffer, selected);
        assert_eq!(state.focus, FocusState::Input);
        press(&mut state, Key::Ctrl('r'));
        press(&mut state, Key::Ctrl('p'));
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::CommandPalette
        );
        press(&mut state, Key::Esc);
        assert_eq!(state.focus, FocusState::Input);
        assert!(state.modal_stack.is_empty());
    }

    #[test]
    fn closed_popup_control_accepts_composer_paste() {
        let mut state = TuiState::new(80, 24);
        app::handle_input(
            &mut state,
            &rustain::domain::events::DomainInputEvent::Paste("visible draft".into()),
        );
        assert_eq!(state.input_buffer, "visible draft");
        assert!(!state.reverse_search.is_open());
        assert!(state.modal_stack.is_empty());
    }
}
