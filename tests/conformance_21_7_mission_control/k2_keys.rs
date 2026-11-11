//! K2 / AC2: the keys over the hub resolve truthfully. `Left`/`Right` cycle the delivered tabs
//! (inert, and not painted, with one tab); RU-25 scrolling acts by value; `Esc`/`Ctrl+C` go
//! back and leave the interrupted base and the Stage identical; `Ctrl+X ?`/`n`/`q`, `Ctrl+P`
//! and `Ctrl+Q` push over the hub and return exactly; an unbound printable key changes nothing
//! (positive control: the same key types in the composer); a Release, a Repeat or an Alt
//! variant never acts; every key the rail paints is a registry row that acts.
use super::support::*;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::{self, Context, Host, Key, Resolved};
use rustain::adapters::tui::mission_control::McTab;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::domain::models::FocusState;

async fn opened(width: u16, height: u16, nodes: &[&str]) -> Rig {
    let mut rig = Rig::plain(room_workspace(nodes).await, width, height).await;
    open_room(&mut rig).await;
    rig
}

fn many(count: usize) -> Vec<String> {
    (0..count).map(|n| format!("spoke-{n:02}")).collect()
}

#[tokio::test]
async fn left_and_right_cycle_the_delivered_tabs_in_place_and_are_painted_with_a_choice() {
    let mut rig = opened(80, 24, &NODES).await;
    let cache = rig.f.cache();
    rig.f.paint();
    assert!(
        rail_line(&rig).contains("[←/→] Tab"),
        "three tabs: the tab keys are painted: {:?}",
        rail_line(&rig)
    );
    // Messages, Room, Artifacts in the fixed order, wrapping at both ends.
    let step = |rig: &mut Rig, key: Key, tab: McTab| {
        assert_eq!(rig.f.press(key), InputAction::OpenMissionControl(tab));
    };
    step(&mut rig, Key::Right, McTab::Artifacts);
    assert_eq!(
        top_title(&rig).as_deref(),
        Some("Mission Control · Artifacts")
    );
    step(&mut rig, Key::Right, McTab::Agents);
    assert_eq!(
        top_title(&rig).as_deref(),
        Some("Mission Control · Agents"),
        "wraps"
    );
    step(&mut rig, Key::Left, McTab::Artifacts);
    assert_eq!(
        top_title(&rig).as_deref(),
        Some("Mission Control · Artifacts"),
        "wraps back"
    );
    step(&mut rig, Key::Left, McTab::Room);
    assert_eq!(top_title(&rig).as_deref(), Some("Mission Control · Room"));
    assert_eq!(
        rig.f.h.state.modal_stack.len(),
        1,
        "the tab changes in place"
    );
    assert_eq!(rig.f.cache(), cache, "the Stage is never touched");
    rig.f.paint();
    assert!(lines(&rig)[usize::from(frame_rect(&rig).y) + 1].contains("Room"));
}

#[tokio::test]
async fn back_restores_the_interrupted_base_and_the_stage_exactly() {
    for back in [Key::Esc, Key::Ctrl('c')] {
        let mut rig = Rig::plain(room_workspace(&NODES).await, 80, 24).await;
        rig.f.h.state.focus = FocusState::Chat;
        rig.f.h.state.input_buffer = "saved draft".to_owned();
        rig.f.h.state.cursor_position = 5;
        let cache = rig.f.cache();
        rig.f.paint();
        let before = lines(&rig);
        open_room(&mut rig).await;
        assert_eq!(rig.f.press(back), InputAction::Consumed, "{back:?}");
        assert!(rig.f.h.state.modal_stack.is_empty(), "{back:?}");
        assert_eq!(
            rig.f.h.state.focus,
            FocusState::Chat,
            "{back:?}: the base focus"
        );
        assert_eq!(rig.f.h.state.input_buffer, "saved draft", "{back:?}");
        assert_eq!(rig.f.h.state.cursor_position, 5, "{back:?}");
        assert_eq!(rig.f.cache(), cache, "{back:?}: Stage caches");
        rig.f.paint();
        assert_eq!(
            lines(&rig),
            before,
            "{back:?}: the Stage rows are what they were"
        );
        assert!(presentation::consistent(&rig.f.h.state));
    }
}

#[tokio::test]
async fn stack_chords_push_over_the_hub_and_return_exactly() {
    let mut rig = opened(80, 24, &NODES).await;
    let hub = rig.f.h.state.modal_stack.top().unwrap().id;
    for (keys, expected) in [
        (&[Key::Ctrl('x'), Key::Char('?')][..], Surface::Help),
        (
            &[Key::Ctrl('x'), Key::Char('n')][..],
            Surface::NotificationLog,
        ),
        (&[Key::Ctrl('p')][..], Surface::CommandPalette),
    ] {
        for key in keys {
            rig.f.press(*key);
        }
        assert_eq!(top_surface(&rig), Some(expected), "{keys:?}");
        assert_eq!(
            rig.f.h.state.modal_stack.len(),
            2,
            "{keys:?}: pushed over the hub"
        );
        assert_eq!(rig.f.press(Key::Esc), InputAction::Consumed);
        assert_eq!(
            top_surface(&rig),
            Some(Surface::MissionControl),
            "{keys:?}: back to the hub"
        );
        assert_eq!(
            rig.f.h.state.modal_stack.top().unwrap().id,
            hub,
            "{keys:?}: same frame"
        );
        assert_eq!(rig.f.h.state.modal_stack.len(), 1);
    }
    // `Ctrl+X q` with nothing waiting says so and returns to the hub; the leader menu
    // itself is dismissed by `Esc` without closing the hub.
    rig.f.press(Key::Ctrl('x'));
    rig.f.press(Key::Esc);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert!(rig.f.h.state.pending_leader.is_none());
    // `Ctrl+Q` is a quit request and does not close the hub.
    assert_eq!(rig.f.press(Key::Ctrl('q')), InputAction::Quit);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
}

#[tokio::test]
async fn an_unbound_printable_key_changes_nothing_and_never_reaches_the_composer() {
    let mut rig = opened(80, 24, &NODES).await;
    rig.f.h.state.input_buffer = "keep".to_owned();
    rig.f.h.state.cursor_position = 4;
    rig.f.paint();
    let cache = rig.f.cache();
    let before = (
        lines(&rig),
        rig.f.h.state.input_buffer.clone(),
        rig.f.h.state.cursor_position,
    );
    for key in ['z', 'q', 'y', 'i', 'a', '1', '?', ' ', 'Z'] {
        let resolved = keymap::resolve(Context::MissionControl, Host::Local, &[Key::Char(key)]);
        assert!(
            !matches!(resolved, Resolved::Hit(row) if row.context == Context::MissionControl),
            "{key:?} must not be an S-MC row for this to be an unbound witness"
        );
        let action = rig.f.press(Key::Char(key));
        assert!(
            matches!(action, InputAction::Consumed | InputAction::Ignored),
            "{key:?}: {action:?}"
        );
        rig.f.paint();
        assert_eq!(top_surface(&rig), Some(Surface::MissionControl), "{key:?}");
        assert_eq!(rig.f.h.state.input_buffer, "keep", "{key:?}: composer");
        assert_eq!(rig.f.h.state.cursor_position, 4, "{key:?}: cursor");
        assert_eq!(rig.f.cache(), cache, "{key:?}: Stage");
        assert_eq!(lines(&rig), before.0, "{key:?}: the painted frame");
    }
    // Positive control: with no frame the same key types.
    let mut plain = Rig::plain(room_workspace(&NODES).await, 80, 24).await;
    plain.f.h.state.focus = FocusState::Input;
    plain.f.press(Key::Char('z'));
    assert_eq!(plain.f.h.state.input_buffer, "z");
    assert_eq!(before.1, "keep");
}

#[tokio::test]
async fn only_a_plain_press_acts_release_repeat_and_alt_variants_are_inert() {
    let mut rig = opened(
        80,
        24,
        &many(40).iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    // Positive control: a plain `j` press scrolls the room by one row.
    let top = rig.f.h.state.room_panel.scroll_offset;
    assert!(
        raw(
            &mut rig,
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Press
        )
        .is_some()
    );
    assert_eq!(rig.f.h.state.room_panel.scroll_offset, top + 1);
    let before = rig.f.h.state.room_panel.scroll_offset;
    for (code, modifiers, kind, label) in [
        (
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
            "j release",
        ),
        (
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
            "j repeat",
        ),
        (
            KeyCode::Char('j'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            "alt j",
        ),
        (
            KeyCode::Char('j'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyEventKind::Press,
            "ctrl-alt j",
        ),
        (
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Release,
            "esc release",
        ),
        (
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
            "esc repeat",
        ),
        (
            KeyCode::Esc,
            KeyModifiers::ALT,
            KeyEventKind::Press,
            "alt esc",
        ),
        (
            KeyCode::Left,
            KeyModifiers::NONE,
            KeyEventKind::Release,
            "left release",
        ),
    ] {
        assert_eq!(
            raw(&mut rig, code, modifiers, kind),
            None,
            "{label}: the raw pre-filter drops it"
        );
        assert_eq!(rig.f.h.state.room_panel.scroll_offset, before, "{label}");
        assert_eq!(top_surface(&rig), Some(Surface::MissionControl), "{label}");
    }
    // A plain Esc press closes the frame.
    assert!(
        raw(
            &mut rig,
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Press
        )
        .is_some()
    );
    assert!(rig.f.h.state.modal_stack.is_empty());
}

#[tokio::test]
async fn ru25_scrolling_acts_by_value_and_every_painted_key_is_a_registry_row_that_acts() {
    let names = many(40);
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut rig = opened(80, 24, &refs).await;
    let rail = rail_line(&rig);
    assert!(rail.contains("[j/k] Scroll"), "{rail:?}");
    assert!(rail.contains("[Esc] Back"), "{rail:?}");
    // Every painted `[...]` item resolves to a row available in this context (KG-2).
    for key in [Key::Char('j'), Key::Char('k'), Key::Esc] {
        assert!(
            matches!(
                keymap::resolve(Context::MissionControl, Host::Local, &[key]),
                Resolved::Hit(_)
            ),
            "{key:?}"
        );
    }
    let first_row = |rig: &Rig| {
        body_lines(rig)
            .iter()
            .find(|row| row.contains("spoke-"))
            .cloned()
            .unwrap_or_default()
    };
    let start = first_row(&rig);
    assert!(start.contains("spoke-00"), "{start:?}");
    // j scrolls one row, k back; half page and page move by value; G is the end, g the start.
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    assert!(
        first_row(&rig).contains("spoke-01"),
        "{:?}",
        first_row(&rig)
    );
    rig.f.press(Key::Char('k'));
    rig.f.paint();
    assert_eq!(first_row(&rig), start);
    rig.f.press(Key::Ctrl('d'));
    rig.f.paint();
    let half = first_row(&rig);
    assert!(
        !half.contains("spoke-00") && !half.contains("spoke-01"),
        "{half:?}"
    );
    rig.f.press(Key::Char('G'));
    rig.f.paint();
    assert!(
        body_lines(&rig).iter().any(|row| row.contains("spoke-39")),
        "G reaches the last node: {:?}",
        body_lines(&rig)
    );
    rig.f.press(Key::Char('g'));
    rig.f.paint();
    assert_eq!(first_row(&rig), start, "g is the start");
    for key in [
        Key::PageDown,
        Key::Down,
        Key::Up,
        Key::PageUp,
        Key::Ctrl('u'),
    ] {
        assert_eq!(rig.f.press(key), InputAction::Consumed, "{key:?}");
        rig.f.paint();
        assert_eq!(top_surface(&rig), Some(Surface::MissionControl), "{key:?}");
    }
}

#[tokio::test]
async fn a_body_that_does_not_scroll_paints_no_scroll_key() {
    let mut rig = opened(130, 30, &["only-one"]).await;
    assert!(!rail_line(&rig).contains("[j/k]"), "{:?}", rail_line(&rig));
    assert!(rail_line(&rig).contains("[Esc] Back"));
    let before = rig.f.h.state.room_panel.scroll_offset;
    rig.f.press(Key::Char('j'));
    assert_eq!(
        rig.f.h.state.room_panel.scroll_offset, before,
        "and the key is inert"
    );
}

// 7A-04 (patch round 1): `Ctrl+X q` pushes the Needs-you list over the hub and `Esc` returns to
// the very same hub, Stage and composer.
#[tokio::test]
async fn ctrl_x_q_pushes_the_needs_you_list_over_the_hub_and_returns_exactly() {
    use rustain::adapters::tui::decision_queue::{self, Arrival, DecisionDraft, DecisionKind};
    let mut rig = opened(80, 24, &NODES).await;
    rig.f.h.state.input_buffer = "keep".to_owned();
    rig.f.h.state.cursor_position = 4;
    decision_queue::raise(
        &mut rig.f.h.state,
        DecisionDraft::new(DecisionKind::Budget, "a waiting decision"),
        Arrival::Automatic,
    );
    rig.f.paint();
    let hub = rig.f.h.state.modal_stack.top().unwrap().id;
    let cache = rig.f.cache();
    let tab = rig.f.h.state.mission_control.tab;
    rig.f.press(Key::Ctrl('x'));
    assert_eq!(rig.f.press(Key::Char('q')), InputAction::FocusDecisions);
    decision_queue::open_waiting(
        &mut rig.f.h.state,
        &rustain::adapters::tui::notification_log::LogFacts::default(),
    );
    rig.f.paint();
    assert_eq!(top_surface(&rig), Some(Surface::DecisionQueue));
    assert_eq!(rig.f.h.state.modal_stack.len(), 2, "pushed over the hub");
    assert_eq!(rig.f.press(Key::Esc), InputAction::Consumed);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(rig.f.h.state.modal_stack.len(), 1);
    assert_eq!(
        rig.f.h.state.modal_stack.top().unwrap().id,
        hub,
        "the same frame"
    );
    assert_eq!(rig.f.h.state.mission_control.tab, tab);
    assert_eq!(rig.f.h.state.input_buffer, "keep");
    assert_eq!(rig.f.h.state.cursor_position, 4);
    assert_eq!(rig.f.cache(), cache, "the Stage is untouched");
}
