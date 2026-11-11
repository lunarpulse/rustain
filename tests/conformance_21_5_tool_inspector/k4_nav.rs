//! K4 / M4 / M5: navigation moves the real body; Stage cache has its own owner.
use super::support::*;
use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
use rustain::adapters::tui::{
    app, app::InputAction, keymap::Key, markdown::metrics, tool_inspector::ToolTab,
};
use rustain::domain::models::{InvocationStatus as S, MouseConfig, WriteDiffState as D};
use serde_json::json;

fn long() -> Fixture {
    // Shared Literal deliberately appends one paragraph separator: 99 + 1 = 100.
    let (mut f, _) = Fixture::single(
        80,
        23,
        "Bash",
        json!({"command":"seq 1 99","input_lines":(0..30).collect::<Vec<_>>()}),
        S::Success,
        Some((&rows(99), D::NotAWrite)),
    );
    f.open();
    assert_eq!(
        f.body_rect().height,
        12,
        "the by-value half/page witness needs a twelve-row body"
    );
    assert_eq!(
        f.tool().rows().len(),
        100,
        "do not change SharedLiteral to satisfy the fixture"
    );
    f
}

fn wheel(f: &mut Fixture, down: bool) {
    let body = f.body_rect();
    let event = Event::Mouse(MouseEvent {
        kind: if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        },
        column: body.x,
        row: body.y,
        modifiers: KeyModifiers::NONE,
    });
    let decoded =
        app::convert_crossterm_event(&event, &MouseConfig::default()).expect("real wheel decoder");
    assert_eq!(
        app::handle_input(&mut f.h.state, &decoded),
        InputAction::Consumed
    );
    f.paint();
}

#[test]
fn half_page_page_and_clamp_by_value() {
    let mut f = long();
    for (key, expected) in [
        (Key::Char('j'), 1),
        (Key::Ctrl('d'), 7),
        (Key::PageDown, 18),
        (Key::Char('G'), 88),
        (Key::Char('j'), 88),
        (Key::Down, 88),
        (Key::Char('k'), 87),
        (Key::Up, 86),
        (Key::Ctrl('u'), 80),
        (Key::PageUp, 69),
        (Key::Char('g'), 0),
        (Key::Char('k'), 0),
        (Key::Up, 0),
        (Key::Down, 1),
        (Key::Char('g'), 0),
    ] {
        assert_eq!(f.press(key), InputAction::Consumed, "{key:?}");
        f.paint();
        f.top_is(expected);
    }
    wheel(&mut f, true);
    f.top_is(3);
    wheel(&mut f, false);
    f.top_is(0);
    wheel(&mut f, false);
    f.top_is(0);
}

#[test]
fn short_body_never_scrolls_and_each_tab_remembers_its_own_top() {
    let (mut short, _) = Fixture::single(
        80,
        23,
        "Bash",
        json!({"command":"short"}),
        S::Success,
        Some((&rows(4), D::NotAWrite)),
    );
    short.open();
    assert_eq!(short.tool().rows().len(), 5);
    for key in [
        Key::Char('j'),
        Key::Down,
        Key::Char('k'),
        Key::Up,
        Key::Ctrl('d'),
        Key::Ctrl('u'),
        Key::PageDown,
        Key::PageUp,
        Key::Char('g'),
        Key::Char('G'),
    ] {
        short.press(key);
        short.paint();
        short.top_is(0);
    }
    wheel(&mut short, true);
    short.top_is(0);
    wheel(&mut short, false);
    short.top_is(0);
    let mut f = long();
    f.press(Key::Ctrl('d'));
    f.paint();
    f.top_is(6);
    let output_pixels = pixels(f.buffer(), f.body_rect());
    f.press(Key::Right);
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Input);
    assert_eq!(f.tool().viewers[ToolTab::Input.index()].top, 0);
    assert!(f.body_row(0).contains('{'));
    f.press(Key::Down);
    f.paint();
    assert_eq!(f.tool().viewers[ToolTab::Input.index()].top, 1);
    let input_pixels = pixels(f.buffer(), f.body_rect());
    assert!(!f.body_row(0).contains('{'));
    f.press(Key::Left);
    f.paint();
    f.top_is(6);
    assert_eq!(pixels(f.buffer(), f.body_rect()), output_pixels);
    f.press(Key::Right);
    f.paint();
    assert_eq!(pixels(f.buffer(), f.body_rect()), input_pixels);
    assert_eq!(f.tool().viewers[ToolTab::Input.index()].top, 1);
}

#[test]
fn body_renders_once_per_tab_key_across_five_paints_and_return() {
    let (mut f, _) = Fixture::single(
        80,
        23,
        "Bash",
        json!({"command":"seq 1 99"}),
        S::Success,
        Some((&rows(99), D::NotAWrite)),
    );
    let stage = f.cache();
    metrics::reset();
    assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
    assert!(f.inspect());
    for _ in 0..5 {
        f.compose();
    }
    assert_eq!(
        metrics::renders(),
        1,
        "Output has one shared-pipeline render per key"
    );
    assert!(f.body_row(0).contains("row 000"));
    f.press(Key::Right);
    f.compose();
    assert_eq!(metrics::renders(), 2);
    assert!(f.body_row(0).contains('{'));
    f.press(Key::Left);
    for _ in 0..5 {
        f.compose();
    }
    assert_eq!(
        metrics::renders(),
        2,
        "returning to Output must reuse its exact memo"
    );
    assert!(f.body_row(0).contains("row 000"));
    assert_eq!(f.tool().heights.message_entries.len(), 2);
    assert!(
        f.tool().heights.entries.is_empty(),
        "the frame never uses turn-shaped keys"
    );
    assert_eq!(f.cache(), stage);
}

#[test]
fn inspector_never_touches_the_stage_cache() {
    let mut f = long();
    f.press(Key::Esc);
    f.paint();
    let initial = f.cache();
    f.tabs.active_tab_mut().conversation.messages.insert(
        0,
        rustain::domain::models::ChatMessage {
            id: "stage-cache-control".into(),
            role: rustain::domain::models::MessageRole::User,
            content: "Stage-owned cache control".into(),
            ..Default::default()
        },
    );
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.paint();
    let stage = f.cache();
    assert_ne!(
        stage, initial,
        "positive control: the actual owning manager's Stage populates its cache"
    );
    assert!(!stage.0.is_empty() && !stage.1.is_empty());
    f.open();
    assert_eq!(f.cache(), stage);
    for key in [
        Key::Char('j'),
        Key::Ctrl('d'),
        Key::PageDown,
        Key::Char('G'),
        Key::Right,
        Key::Down,
        Key::Right,
        Key::PageDown,
        Key::Left,
        Key::Left,
        Key::Char('g'),
    ] {
        f.press(key);
        f.paint();
        assert_eq!(
            f.cache(),
            stage,
            "{key:?} changed an actual Stage key or entry"
        );
    }
    wheel(&mut f, true);
    assert_eq!(f.cache(), stage);
    wheel(&mut f, false);
    assert_eq!(f.cache(), stage);
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.cache(), stage);
}

fn unpainted() -> Fixture {
    let (mut f, _) = Fixture::single(
        80,
        23,
        "Bash",
        json!({"input_lines": (0..99).collect::<Vec<_>>()}),
        S::Success,
        Some((&rows(99), D::NotAWrite)),
    );
    assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
    assert!(f.inspect(), "open through the production host effect");
    assert!(
        f.tool().rows().is_empty(),
        "control: no inspector paint yet"
    );
    f
}

#[test]
fn prepaint_keyboard_intent_uses_the_prepared_body() {
    for (keys, expected) in [
        (vec![Key::Char('j')], 1),
        (vec![Key::Ctrl('d')], 6),
        (vec![Key::PageDown], 11),
        (vec![Key::Char('G')], 88),
        (vec![Key::Char('G'), Key::Char('k')], 87),
        (vec![Key::Char('G'), Key::Char('g'), Key::Down], 1),
    ] {
        let mut f = unpainted();
        for key in keys {
            assert_eq!(f.press(key), InputAction::Consumed);
        }
        f.paint();
        f.top_is(expected);
    }
}

#[test]
fn prepaint_tab_navigation_retains_ordered_per_tab_intent() {
    let mut f = unpainted();
    for key in [
        Key::Char('G'),
        Key::Right,
        Key::Char('G'),
        Key::Left,
        Key::Char('k'),
    ] {
        assert_eq!(f.press(key), InputAction::Consumed);
    }
    f.paint();
    f.top_is(87);
    f.press(Key::Right);
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Input);
    let expected = f.tool().rows().len().saturating_sub(f.tool().viewport);
    assert!(
        expected > 0,
        "control: Input is a scrollable, newly prepared body"
    );
    assert_eq!(f.tool().viewers[ToolTab::Input.index()].top, expected);
    let expected_pixels: String = f.tool().rows()[expected]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(f.body_row(0).trim_end(), expected_pixels.trim_end());
}

#[test]
fn prepaint_wheel_intent_uses_the_prepared_body() {
    let mut f = unpainted();
    let body = f.body_rect();
    for kind in [
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollUp,
    ] {
        let event = Event::Mouse(MouseEvent {
            kind,
            column: body.x,
            row: body.y,
            modifiers: KeyModifiers::NONE,
        });
        let decoded = app::convert_crossterm_event(&event, &MouseConfig::default()).unwrap();
        assert_eq!(
            app::handle_input(&mut f.h.state, &decoded),
            InputAction::Consumed
        );
    }
    f.paint();
    f.top_is(3);
}
