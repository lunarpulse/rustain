//! K2 — the `Ctrl+X` leader from the registry through the local front door
//! (AC2, KG-6). Attached legs: `cargo test --lib story21_3_attached_` in
//! `attach_keys.rs`. Effects are asserted by value (returned action, state,
//! recorded notices), never "the resolver returned X".

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::keymap::table::PANEL_NARROW;
use rustain::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource};
use rustain::adapters::tui::presentation::{Surface, is_open};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::{DensityMode, PanelType};

use super::support::{is_inert, press};

/// A wide state: panels are available (>= 120 columns).
fn wide() -> TuiState {
    let mut state = TuiState::new(130, 30);
    state.density_mode = DensityMode::Monitor;
    state
}

fn leader(state: &mut TuiState, follow_up: Key) -> InputAction {
    assert_eq!(press(state, Key::Ctrl('x')), InputAction::Consumed);
    assert!(state.pending_leader.is_some(), "Ctrl+X opens the leader");
    press(state, follow_up)
}

fn notices(state: &TuiState) -> Vec<(NoticeSeverity, String)> {
    state
        .notifications
        .records()
        .map(|r| (r.severity, r.message.clone()))
        .collect()
}

#[test]
fn every_leader_follow_up_runs_its_command_by_value() {
    let panels = [
        ('a', PanelType::Adapters),
        ('s', PanelType::Agents),
        ('l', PanelType::TransparencyLog),
        ('r', PanelType::Room),
        ('e', PanelType::Artifacts),
        ('t', PanelType::Tasks),
        ('h', PanelType::History),
    ];
    for (key, panel) in panels {
        let mut state = wide();
        assert_eq!(
            leader(&mut state, Key::Char(key)),
            InputAction::OpenPanel(panel),
            "{key}"
        );
        assert!(state.pending_leader.is_none(), "{key}: the leader closed");
        assert_eq!(
            state.focus,
            FocusState::Input,
            "{key}: origin restored first"
        );
        assert!(notices(&state).is_empty(), "{key}: no notice on a run");
    }
    let mut state = wide();
    assert_eq!(
        leader(&mut state, Key::Char('p')),
        InputAction::OpenProfileSwitcher
    );
    let mut state = wide();
    assert_eq!(
        leader(&mut state, Key::Char('m')),
        InputAction::OpenModelSelector
    );
    let mut state = wide();
    assert!(matches!(
        leader(&mut state, Key::Char('i')),
        InputAction::Login(_)
    ));
    let mut state = wide();
    assert_eq!(
        leader(&mut state, Key::Char('u')),
        InputAction::OpenUsagePanel
    );
    let mut state = wide();
    assert_eq!(
        leader(&mut state, Key::Char('q')),
        InputAction::FocusDecisions
    );

    // `w` toggles Monitor: Focus -> Monitor, Monitor -> Focus (A6).
    let mut state = TuiState::new(130, 30);
    assert_eq!(state.density_mode, DensityMode::Focus);
    assert_eq!(
        leader(&mut state, Key::Char('w')),
        InputAction::SetDensityMode(DensityMode::Monitor)
    );
    let mut state = wide();
    assert_eq!(
        leader(&mut state, Key::Char('w')),
        InputAction::SetDensityMode(DensityMode::Focus)
    );
    // Dashboard -> Monitor (the toggle leaves every non-Monitor density for Monitor).
    let mut state = TuiState::new(130, 30);
    state.density_mode = DensityMode::Dashboard;
    assert_eq!(
        leader(&mut state, Key::Char('w')),
        InputAction::SetDensityMode(DensityMode::Monitor)
    );

    // `?` opens Help over the origin; `n` opens the Notification Log.
    let mut state = wide();
    assert_eq!(leader(&mut state, Key::Char('?')), InputAction::Consumed);
    assert!(is_open(&state, Surface::Help));
    assert_eq!(state.focus, FocusState::Input);
    let mut state = wide();
    assert_eq!(leader(&mut state, Key::Char('n')), InputAction::Consumed);
    assert!(state.notification_log.is_some());

    // `x` archives every visible toast.
    let mut state = wide();
    notifications::info(&mut state, NoticeSource::System, "toast");
    assert_eq!(state.notifications.visible().count(), 1);
    assert_eq!(leader(&mut state, Key::Char('x')), InputAction::Consumed);
    assert_eq!(state.notifications.visible().count(), 0);
    assert_eq!(state.notifications.records().count(), 1, "the log keeps it");

    // Letters are case-insensitive: `Ctrl+X E` = `Ctrl+X e`.
    let mut state = wide();
    assert_eq!(
        leader(&mut state, Key::Char('E')),
        InputAction::OpenPanel(PanelType::Artifacts)
    );
}

#[test]
fn enter_sends_only_from_an_input_origin() {
    let mut state = wide();
    state.input_buffer = "hello".into();
    state.cursor_position = 5;
    assert_eq!(
        leader(&mut state, Key::Enter),
        InputAction::SubmitMessage("hello".into())
    );
    // Empty composer: nothing to send, and no notice either.
    let mut empty = wide();
    assert_eq!(leader(&mut empty, Key::Enter), InputAction::Consumed);
    assert!(notices(&empty).is_empty());
    // Chat origin: Enter has no binding here.
    let mut chat = wide();
    chat.focus = FocusState::Chat;
    assert_eq!(leader(&mut chat, Key::Enter), InputAction::Consumed);
    assert_eq!(
        notices(&chat),
        vec![(
            NoticeSeverity::Info,
            "No binding for Ctrl+X Enter in Chat".to_string()
        )]
    );
    assert_eq!(chat.focus, FocusState::Chat);
}

#[test]
fn unknown_follow_up_posts_no_binding_toast() {
    // Letters, digits, Tab, arrows, function keys, Ctrl/Alt letters, and the
    // retired local `f`/`d` each post exactly one info record; the origin focus
    // is restored and the composer is untouched.
    let unknown: [(Key, &str); 10] = [
        (Key::Char('j'), "j"),
        (Key::Char('5'), "5"),
        (Key::Char('f'), "f"),
        (Key::Char('d'), "d"),
        (Key::Tab, "Tab"),
        (Key::Left, "Left"),
        (Key::F(2), "F2"),
        (Key::Ctrl('a'), "Ctrl+A"),
        (Key::Ctrl('j'), "Ctrl+J"),
        (Key::Alt('z'), "Alt+Z"),
    ];
    for (key, label) in unknown {
        let mut state = wide();
        state.input_buffer = "keep".into();
        state.cursor_position = 4;
        let action = leader(&mut state, key);
        assert!(is_inert(&action), "{label}: {action:?}");
        assert!(state.pending_leader.is_none(), "{label}: the leader closed");
        assert_eq!(state.focus, FocusState::Input, "{label}: origin restored");
        assert_eq!(state.input_buffer, "keep", "{label}: composer unchanged");
        assert_eq!(
            notices(&state),
            vec![(
                NoticeSeverity::Info,
                format!("No binding for Ctrl+X {label} in Input")
            )],
            "{label}"
        );
    }
    // Positive control: a bound key posts no such notice.
    let mut state = wide();
    let _ = leader(&mut state, Key::Char('p'));
    assert!(notices(&state).is_empty());
}

#[test]
fn escape_and_ctrl_c_cancel_the_leader_silently() {
    for key in [Key::Esc, Key::Ctrl('c')] {
        let mut state = wide();
        state.focus = FocusState::Chat;
        let action = leader(&mut state, key);
        assert_eq!(action, InputAction::Consumed, "{key:?}: never CancelOrQuit");
        assert!(state.pending_leader.is_none());
        assert_eq!(state.focus, FocusState::Chat);
        assert!(notices(&state).is_empty(), "{key:?}: silent");
    }
}

#[test]
fn width_unavailable_panel_answers_with_its_remedy() {
    // 80 columns: the sidebar column cannot fit, so the panel chord is `(n/a)`
    // and the key answers with the remedy instead of moving focus invisibly.
    for key in ['a', 's', 'l', 'r', 'e', 't', 'h'] {
        let mut state = TuiState::new(80, 24);
        let action = leader(&mut state, Key::Char(key));
        assert!(is_inert(&action), "{key}: {action:?}");
        assert_eq!(state.focus, FocusState::Input, "{key}: focus unchanged");
        assert_eq!(
            notices(&state),
            vec![(NoticeSeverity::Info, PANEL_NARROW.to_string())],
            "{key}"
        );
    }
    // Control: the Usage overlay needs no sidebar column, so it still runs.
    let mut state = TuiState::new(80, 24);
    assert_eq!(
        leader(&mut state, Key::Char('u')),
        InputAction::OpenUsagePanel
    );
}

#[test]
fn the_leader_is_untimed_across_ticks() {
    let mut state = wide();
    assert_eq!(press(&mut state, Key::Ctrl('x')), InputAction::Consumed);
    for _ in 0..500 {
        notifications::tick(&mut state);
    }
    assert!(state.pending_leader.is_some(), "no timeout closes the menu");
    assert_eq!(
        press(&mut state, Key::Char('h')),
        InputAction::OpenPanel(PanelType::History)
    );
}
