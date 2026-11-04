//! K10 (local leg, AC13, RU-20): `Ctrl+C` inside a modal surface is back one
//! frame. Each surface is opened through its real key (or its state API when the
//! event loop opens it), then `Ctrl+C` must behave exactly like `Esc` on an
//! identical fixture, never return `CancelOrQuit`, and close what closes in state.
//! The attached leg is `story21_3_attached_ctrl_c_in_overlay_never_interrupts`.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::notification_log::{self, LogFacts};
use rustain::adapters::tui::presentation::{Surface, is_open, reducer};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;

use super::k4_overlays::{cross_search_state, recipient};
use super::support::{fresh, press};

/// Everything a surface can leave open, in one comparable string.
fn open_surfaces(s: &TuiState) -> String {
    format!(
        "{:?} help={} palette={} model={} profile={} usage={} reverse={} popup={} log={} surface={} frames={:?}",
        s.focus,
        is_open(s, Surface::Help),
        is_open(s, Surface::CommandPalette),
        is_open(s, Surface::ModelSelector),
        is_open(s, Surface::ProfileSwitcher),
        is_open(s, Surface::UsagePanel),
        s.reverse_search.is_open(),
        s.autocomplete.is_open(),
        s.notification_log.is_some(),
        s.recipient_surface.is_some(),
        s.modal_stack.iter().collect::<Vec<_>>(),
    )
}

type Open = fn(&mut TuiState);

/// (name, how it opens, `None` when Ctrl+C leaves nothing open in state, else the exact
/// action the host closes it with: by value, never `CancelOrQuit`).
const SURFACES: &[(&str, Open, Option<InputAction>)] = &[
    (
        "Help",
        |s| {
            press(s, Key::Ctrl('x'));
            press(s, Key::Char('?'));
        },
        None,
    ),
    (
        "CommandPalette",
        |s| {
            press(s, Key::Ctrl('p'));
        },
        None,
    ),
    (
        "ModelSelector",
        |s| {
            reducer::open(s, Surface::ModelSelector);
            s.model_selector.initialize(vec![], "p", "m");
        },
        None,
    ),
    (
        "ProfileSwitcher",
        |s| {
            rustain::adapters::tui::handlers::profile_switch::open(s, |selector| {
                selector.initialize(vec![], "x", Default::default());
            });
        },
        None,
    ),
    (
        "UsagePanel",
        |s| {
            reducer::open(s, Surface::UsagePanel);
            s.usage_panel.initialize();
        },
        None,
    ),
    (
        "Search",
        |s| {
            press(s, Key::Ctrl('f'));
        },
        Some(InputAction::CloseSearch),
    ),
    (
        "CrossSearch",
        |s| *s = cross_search_state(),
        Some(InputAction::CloseCrossSearch),
    ),
    (
        "BookmarkList",
        |s| {
            reducer::open(s, Surface::BookmarkList);
        },
        Some(InputAction::CloseBookmarkList),
    ),
    (
        "WaveOverlay",
        |s| {
            s.focus = FocusState::Chat;
            s.wave_run = Some(std::sync::Arc::new(super::support::StubWave(3)));
            let action = press(s, Key::Char('r'));
            rustain::adapters::tui::handlers::wave_overlay::apply(s, "", &action);
        },
        Some(InputAction::CloseWaveOverlay),
    ),
    (
        "ReverseSearch",
        |s| {
            press(s, Key::Ctrl('r'));
        },
        None,
    ),
    (
        "Autocomplete",
        |s| {
            press(s, Key::Char('/'));
        },
        None,
    ),
    ("RecipientSurface", |s| *s = recipient(), None),
];

/// Regression: Help and ReverseSearch returned `CancelOrQuit` (stopping a turn or
/// asking to quit), Autocomplete dismissed AND returned it, and Search, CrossSearch,
/// BookmarkList, WaveOverlay and ProfileSwitcher swallowed Ctrl+C without going back.
#[test]
fn ctrl_c_in_every_overlay_goes_back_one_frame() {
    for (name, open, closes) in SURFACES {
        let mut by_esc = fresh();
        open(&mut by_esc);
        let mut by_ctrl_c = fresh();
        open(&mut by_ctrl_c);
        assert_eq!(
            open_surfaces(&by_esc),
            open_surfaces(&by_ctrl_c),
            "{name}: same fixture"
        );
        let esc = press(&mut by_esc, Key::Esc);
        let ctrl_c = press(&mut by_ctrl_c, Key::Ctrl('c'));
        assert_ne!(
            ctrl_c,
            InputAction::CancelOrQuit,
            "{name}: never stops a turn or quits"
        );
        assert_eq!(ctrl_c, esc, "{name}: the same outcome as Esc");
        assert_eq!(
            open_surfaces(&by_ctrl_c),
            open_surfaces(&by_esc),
            "{name}: the same state"
        );
        match closes {
            None => assert_eq!(
                open_surfaces(&by_ctrl_c),
                open_surfaces(&fresh()),
                "{name}: closed, caller restored"
            ),
            Some(action) => assert_eq!(&ctrl_c, action, "{name}: the host closes it"),
        }
        if *name == "CrossSearch" {
            assert!(is_open(&by_ctrl_c, Surface::CrossSearch));
            assert_eq!(by_ctrl_c.modal_stack.len(), 1);
            assert_eq!(
                by_ctrl_c.modal_stack.top().unwrap().surface,
                Surface::CrossSearch,
                "the emitted close effect still owns its frame"
            );
            let base = by_ctrl_c.focus.clone();
            rustain::adapters::tui::handlers::export::close_cross_search(&mut by_ctrl_c);
            assert!(!is_open(&by_ctrl_c, Surface::CrossSearch));
            assert!(by_ctrl_c.modal_stack.is_empty());
            assert_eq!(by_ctrl_c.focus, base, "History caller restored");
        } else if *name == "BookmarkList" {
            assert!(!is_open(&by_ctrl_c, Surface::BookmarkList));
        } else if *name == "WaveOverlay" {
            assert!(
                is_open(&by_ctrl_c, Surface::WaveOverlay),
                "the emitted effect still owns its frame"
            );
            rustain::adapters::tui::handlers::wave_overlay::apply(&mut by_ctrl_c, "", &ctrl_c);
            assert!(!is_open(&by_ctrl_c, Surface::WaveOverlay));
        }
    }
}

/// The host-neutral S-LOG handler runs before `handle_input` in the event loop.
#[test]
fn ctrl_c_in_the_notification_log_closes_it_and_restores_the_caller() {
    let mut state = fresh();
    state.focus = FocusState::Chat;
    rustain::adapters::tui::notifications::info(
        &mut state,
        rustain::adapters::tui::notifications::NoticeSource::Chord,
        "one",
    );
    notification_log::open(&mut state);
    assert!(state.notification_log.is_some());
    let event = DomainInputEvent::SpecialKey(DomainKey::CtrlC);
    let outcome = notification_log::handle_domain_event(&mut state, &event, &LogFacts::default());
    assert!(outcome.is_some(), "the log owns the key");
    assert!(state.notification_log.is_none(), "closed");
    assert_eq!(state.focus, FocusState::Chat, "caller restored");
}

/// The leader menu: Ctrl+C cancels it silently and restores the origin.
#[test]
fn ctrl_c_in_the_leader_menu_cancels_it_silently() {
    let mut state = fresh();
    press(&mut state, Key::Ctrl('x'));
    assert_eq!(press(&mut state, Key::Ctrl('c')), InputAction::Consumed);
    assert!(state.pending_leader.is_none());
    assert_eq!(state.focus, FocusState::Input);
    assert_eq!(state.notifications.records().count(), 0);
}

/// Positive control: with no overlay open Ctrl+C still stops work / asks to quit.
#[test]
fn ctrl_c_with_no_overlay_is_unchanged() {
    assert_eq!(
        press(&mut fresh(), Key::Ctrl('c')),
        InputAction::CancelOrQuit
    );
}
