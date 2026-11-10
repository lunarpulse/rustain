//! K2 (local half, AC3/AC4/AC9): while the thread frame is open the leader, palette and
//! quit chords work over it, the body keys move painted rows by value, every other key
//! (unbound, modified, repeated, released) is consumed with no effect, nothing reaches
//! the Stage or the composer, `Esc` restores the pre-open base focus, a 0->1 decision
//! arrival preempts the frame and `Esc` restores the thread exactly, and the floor
//! terminal keeps title, one body row and one rail row.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{self, INERT_AFTER_PAINT};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::domain::models::{FocusState, ImageAttachment};

use super::support::*;
use crate::conformance_21_13_block_viewer::support::{focus, mixed_turn};

fn image() -> ImageAttachment {
    ImageAttachment {
        media_type: "image/png".into(),
        data: "AAAA".into(),
    }
}

fn composer(rig: &Rig) -> (String, usize, usize) {
    let s = &rig.f.h.state;
    (
        s.input_buffer.clone(),
        s.cursor_position,
        s.input_scroll_offset,
    )
}

/// Everything a dead key must leave alone.
fn snapshot(rig: &Rig) -> String {
    let s = &rig.f.h.state;
    format!(
        "{} {:?} {:?} {:?} {:?} {} {:?}",
        rig.f.view(),
        composer(rig),
        (&s.focus, &s.focused_item_id),
        rig.inspector().viewer,
        rig.depth(),
        s.pending_z,
        s.pending_leader,
    )
}

fn consumed(action: InputAction) -> bool {
    matches!(action, InputAction::Consumed | InputAction::Ignored)
}

#[tokio::test]
async fn k2_leader_palette_and_quit_chords_work_over_the_frame_and_exits_return_to_it() {
    let mut rig = Rig::plain(consultation_workspace().await, 100, 30).await;
    rig.view("ri_c").await;
    rig.paint();
    assert!(rig.is_open());
    let page = (rig.inspector().content.clone(), rig.viewer_top());
    let title = rig.f.h.state.modal_stack.top().unwrap().title().to_owned();
    let back_on_page = |rig: &mut Rig| {
        rig.paint();
        assert!(rig.is_open(), "the thread is the one frame again");
        assert_eq!(rig.f.h.state.modal_stack.top().unwrap().title(), title);
        assert_eq!((rig.inspector().content.clone(), rig.viewer_top()), page);
    };

    // Ctrl+X n -> S-LOG on top; Esc -> the same page and viewer.
    rig.key(Key::Ctrl('x'));
    assert!(
        rig.f.h.state.pending_leader.is_some(),
        "the leader opens over the frame"
    );
    rig.key(Key::Char('n'));
    assert_eq!(rig.depth(), 2);
    assert_eq!(rig.top(), Surface::NotificationLog);
    rig.paint();
    assert!(presentation::consistent(&rig.f.h.state));
    rig.key(Key::Esc);
    back_on_page(&mut rig);

    // Ctrl+X ? -> help over the frame.
    rig.keys(&[Key::Ctrl('x'), Key::Char('?')]);
    assert_eq!(rig.top(), Surface::Help);
    assert_eq!(rig.depth(), 2);
    rig.key(Key::Esc);
    back_on_page(&mut rig);

    // Ctrl+X q -> the Needs-you leader row reaches its host action; the frame stays below.
    rig.key(Key::Ctrl('x'));
    assert_eq!(rig.key(Key::Char('q')), InputAction::FocusDecisions);
    assert!(rig.f.h.state.pending_leader.is_none());
    back_on_page(&mut rig);

    // Ctrl+P -> palette over the frame.
    rig.key(Key::Ctrl('p'));
    assert_eq!(rig.top(), Surface::CommandPalette);
    assert_eq!(rig.depth(), 2);
    rig.key(Key::Esc);
    back_on_page(&mut rig);

    // Ctrl+Q -> the quit request (the host raises S-QUIT with its facts); frame untouched.
    assert_eq!(rig.key(Key::Ctrl('q')), InputAction::Quit);
    back_on_page(&mut rig);

    // Ctrl+C backs exactly one frame, like Esc.
    assert!(consumed(rig.key(Key::Ctrl('c'))));
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert!(rig.f.h.state.thread_inspector.is_none());
}

#[tokio::test]
async fn k2_body_keys_move_painted_rows_by_value_and_the_sideways_keys_are_dead() {
    let mut rig = Rig::plain(long_workspace().await, 80, 24).await;
    rig.view(NEWEST).await;
    rig.paint();
    let reference = painted_body(&mut rig);
    let (rows, viewport) = rig.fit();
    let max = rows - viewport;
    assert_eq!(reference.len(), rows);
    assert!(max > viewport, "the thread must scroll several pages");

    let half = (viewport / 2).max(1);
    let page = (viewport - 1).max(1);
    let steps: [(Key, usize); 12] = [
        (Key::Char('j'), 1),
        (Key::Down, 2),
        (Key::Char('k'), 1),
        (Key::Up, 0),
        (Key::Up, 0),
        (Key::Ctrl('d'), half),
        (Key::Ctrl('u'), 0),
        (Key::PageDown, page),
        (Key::PageUp, 0),
        (Key::Char('G'), max),
        (Key::Char('j'), max),
        (Key::Char('g'), 0),
    ];
    rig.key(Key::Char('g'));
    rig.paint();
    for (key, expected) in steps {
        assert!(consumed(rig.key(key)), "{key:?}");
        rig.paint();
        assert_eq!(rig.viewer_top(), expected, "{key:?}");
        let painted = body_rows(&rig);
        assert_eq!(
            painted[0], reference[expected],
            "first painted row after {key:?}"
        );
        assert_eq!(
            painted[viewport - 1],
            reference[expected + viewport - 1],
            "last painted row after {key:?}"
        );
    }

    // h 0 $ v: consumed no-ops that paint nothing; a draft and the viewer stay put.
    rig.f.h.state.input_buffer = "kept".into();
    rig.f.h.state.cursor_position = 2;
    let at_top = snapshot(&rig);
    rig.key(Key::Char('j'));
    rig.paint();
    let moved = snapshot(&rig);
    assert_ne!(
        moved, at_top,
        "positive control: a plain j moves the viewer"
    );
    let body = body_rows(&rig);
    let footer = footer_rows(&rig);
    for c in ['h', '0', '$'] {
        assert!(consumed(rig.key(Key::Char(c))), "{c}");
        rig.paint();
        assert_eq!(snapshot(&rig), moved, "{c} is a no-op");
        assert_eq!(body_rows(&rig), body, "{c} repaints no body change");
        assert_eq!(footer_rows(&rig), footer, "{c} is not a painted action");
    }
    let rail = compact(&footer.join("\n"));
    for label in ["[h]", "[0]", "[$]"] {
        assert!(!rail.contains(label), "{label} is never painted");
    }
    // 8-B: `v` is the Rendered/Source toggle (painted in the rail), no longer a no-op: it
    // repaints the body in the other mode and back again, leaving the draft and viewer alone.
    assert!(rail.contains("[v]"), "[v] is a painted row in 8-B: {rail}");
    assert!(consumed(rig.key(Key::Char('v'))));
    rig.paint();
    assert_ne!(body_rows(&rig), body, "v switches the body to its source");
    assert!(consumed(rig.key(Key::Char('v'))));
    rig.paint();
    assert_eq!(
        body_rows(&rig),
        body,
        "a second v returns to the rendered body"
    );
    assert_eq!(
        snapshot(&rig),
        moved,
        "v touches neither the draft nor the viewer"
    );
    assert!(
        rail.contains("Scroll"),
        "positive control: the scroll rail is painted"
    );
}

#[tokio::test]
async fn k2_newer_is_offered_only_when_an_omitted_page_exists() {
    let mut rig = Rig::plain(long_workspace().await, 100, 30).await;
    rig.view(NEWEST).await;
    rig.paint();
    assert!(
        rig.inspector().omitted_before > 0,
        "the newest page omits older turns"
    );
    assert_eq!(rig.inspector().omitted_after, 0);
    let rail = compact(&footer_rows(&rig).join("\n"));
    assert!(
        rail.contains("[p]Earlier"),
        "positive control: Earlier is painted: {rail}"
    );
    assert!(
        !rail.contains("Newer"),
        "no later page: Newer is not painted"
    );
    let before = snapshot(&rig);
    assert!(
        consumed(rig.key(Key::Char('l'))),
        "l is inert without a later page"
    );
    assert_eq!(snapshot(&rig), before);
    assert_eq!(
        rig.key(Key::Char('p')),
        InputAction::ThreadPage { earlier: true },
        "p asks the host for the earlier page"
    );

    rig.navigate(true).await;
    rig.paint();
    assert!(rig.inspector().omitted_after > 0);
    assert!(compact(&footer_rows(&rig).join("\n")).contains("[l]Newer"));
    assert_eq!(
        rig.key(Key::Char('l')),
        InputAction::ThreadPage { earlier: false }
    );
}

#[tokio::test]
async fn k2_unbound_and_modified_keys_are_consumed_with_no_effect() {
    let mut rig = Rig::plain(long_workspace().await, 100, 30).await;
    rig.f.h.state.focus = FocusState::Input;
    rig.f.h.state.input_buffer = "draft text".into();
    rig.f.h.state.cursor_position = 3;
    rig.view(NEWEST).await;
    rig.paint();
    let before = snapshot(&rig);
    let events = [
        press_char('z'),
        raw(KeyCode::Char('b'), KeyModifiers::ALT, KeyEventKind::Press),
        raw(KeyCode::Char('r'), KeyModifiers::NONE, KeyEventKind::Repeat),
        raw(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ),
        raw(KeyCode::Char('N'), KeyModifiers::SHIFT, KeyEventKind::Press),
    ];
    for event in events {
        let label = format!("{event:?}");
        assert!(consumed(rig.send(event)), "{label}");
        rig.paint();
        assert_eq!(snapshot(&rig), before, "{label} changed state");
        assert_eq!(
            composer(&rig).0,
            "draft text",
            "{label} reached the composer"
        );
        assert!(rig.is_open(), "{label} left the frame");
    }
    // Positive control: a plain Press of j through the same path scrolls.
    rig.send(press_char('j'));
    rig.paint();
    assert_ne!(snapshot(&rig), before);
    assert_eq!(rig.viewer_top(), 1);
    assert_eq!(composer(&rig).0, "draft text");
}

#[tokio::test]
async fn k2_stage_tool_keys_under_the_frame_open_nothing() {
    let (turn, ids) = mixed_turn();
    let mut rig = Rig::new(consultation_workspace().await, 100, 30, turn).await;
    focus(&mut rig.f, &ids[1]);
    assert_eq!(
        rig.f.h.state.focused_item_id.as_deref(),
        Some(ids[1].as_str())
    );
    rig.view("ri_c").await;
    rig.paint();
    assert!(rig.is_open());
    for key in [Key::Tab, Key::Enter] {
        let action = rig.key(key);
        let shown = format!("{action:?}");
        assert!(consumed(action), "{key:?} -> {shown}");
        rig.paint();
        assert!(rig.is_open(), "{key:?} must not open or close anything");
        assert!(rig.f.h.state.tool_inspectors.is_empty(), "{key:?}");
        assert!(rig.f.h.state.block_viewers.is_empty(), "{key:?}");
        assert_eq!(
            rig.f.h.state.focused_item_id.as_deref(),
            Some(ids[1].as_str())
        );
    }
    // Positive control: with the frame closed the same keys act on the Stage again.
    rig.key(Key::Esc);
    rig.paint();
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert_eq!(rig.key(Key::Enter), InputAction::InspectTool);
    assert_eq!(rig.key(Key::Tab), InputAction::CycleInvocationInFocusedTurn);
}

#[tokio::test]
async fn k2_esc_restores_the_pre_open_base_focus() {
    for focus_before in [FocusState::Input, FocusState::Chat] {
        let label = format!("{focus_before:?}");
        let mut rig = Rig::plain(consultation_workspace().await, 100, 30).await;
        rig.f.h.state.focus = focus_before;
        rig.view("ri_c").await;
        rig.paint();
        assert!(rig.is_open());
        assert_eq!(
            format!("{:?}", rig.f.h.state.focus),
            label,
            "opening writes no focus"
        );
        assert!(consumed(rig.key(Key::Esc)));
        rig.paint();
        assert!(rig.f.h.state.modal_stack.is_empty());
        assert_eq!(
            format!("{:?}", rig.f.h.state.focus),
            label,
            "Esc restores {label}"
        );
    }
}

#[tokio::test]
async fn k2_a_zero_to_one_decision_preempts_the_frame_and_esc_restores_the_thread_exactly() {
    let mut rig = Rig::plain(long_workspace().await, 100, 30).await;
    rig.f.h.state.focus = FocusState::Input;
    rig.view(NEWEST).await;
    rig.paint();
    rig.keys(&[Key::PageDown, Key::PageDown]);
    rig.paint();
    let frames: Vec<_> = rig.f.h.state.modal_stack.iter().cloned().collect();
    let target = format!("{:?}", rig.inspector().target);
    let (page_before, viewer_before) = (
        (
            rig.inspector().first_non_pinned_ordinal,
            rig.inspector().omitted_before,
            rig.inspector().content.clone(),
        ),
        rig.inspector().viewer,
    );
    assert!(
        viewer_before.top > 0,
        "the viewer is scrolled before the arrival"
    );

    let now = rig.f.h.state.notifications.now();
    assert!(
        presentation::arrival_may_preempt(&rig.f.h.state, now),
        "positive control: an empty queue and composer are eligible"
    );
    decision_queue::raise_large_image(&mut rig.f.h.state, image(), "Image is large (6 MB).");
    assert_eq!(rig.depth(), 2, "the decision took the frame");
    assert_eq!(rig.top(), Surface::Decision);
    rig.paint();
    rig.f
        .clock
        .advance(INERT_AFTER_PAINT + Duration::from_millis(1));
    assert!(consumed(rig.key(Key::Esc)), "Esc defers the decision");
    rig.paint();

    assert_eq!(
        rig.f
            .h
            .state
            .modal_stack
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        frames,
        "exactly the interrupted frame is back"
    );
    assert!(rig.is_open());
    assert_eq!(
        format!("{:?}", rig.inspector().target),
        target,
        "same target"
    );
    assert_eq!(
        (
            rig.inspector().first_non_pinned_ordinal,
            rig.inspector().omitted_before,
            rig.inspector().content.clone(),
        ),
        page_before,
        "same page"
    );
    assert_eq!(rig.inspector().viewer, viewer_before, "same viewer");
    // The deferred item is still waiting: a later arrival is 1->2 and never preempts.
    assert_eq!(decision_queue::waiting(&rig.f.h.state), 1);
    let now = rig.f.h.state.notifications.now();
    assert!(!presentation::arrival_may_preempt(&rig.f.h.state, now));
}

#[tokio::test]
async fn k2_an_arrival_with_a_draft_in_the_composer_queues_behind_the_frame() {
    let mut rig = Rig::plain(consultation_workspace().await, 100, 30).await;
    rig.f.h.state.focus = FocusState::Input;
    rig.f.h.state.input_buffer = "typing".into();
    rig.f.h.state.cursor_position = 6;
    rig.view("ri_c").await;
    rig.paint();
    let now = rig.f.h.state.notifications.now();
    assert!(!presentation::arrival_may_preempt(&rig.f.h.state, now));
    // A system arrival (RU-6): a requested one (the user's own action) takes focus at once.
    decision_queue::raise(
        &mut rig.f.h.state,
        decision_queue::DecisionDraft::new(
            decision_queue::DecisionKind::LargeImage,
            "Image is large (6 MB).",
        )
        .subject(decision_queue::DecisionSubject::LargeImage(image())),
        decision_queue::Arrival::Automatic,
    );
    assert_eq!(rig.depth(), 1, "the frame is not preempted");
    assert!(rig.is_open());
    assert_eq!(
        decision_queue::waiting(&rig.f.h.state),
        1,
        "the item waits in the queue"
    );
}

#[tokio::test]
async fn k2_the_floor_terminal_with_a_full_composer_keeps_title_body_and_rail() {
    let mut rig = Rig::plain(consultation_workspace().await, 60, 16).await;
    let empty_height = rig.f.layout().input_area.height;
    rig.f.h.state.input_buffer = (0..12)
        .map(|n| format!("composer line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    rig.f.h.state.cursor_position = rig.f.h.state.input_buffer.len();
    assert!(
        rig.f.layout().input_area.height > empty_height,
        "positive control: the composer grew"
    );
    rig.view("ri_c").await;
    rig.paint();
    assert!(rig.is_open());
    assert!(
        title_row(&rig.f).contains("Thread · consultation"),
        "title row"
    );
    let g = geometry(&rig);
    assert!(g.body >= 1, "at least one body row");
    assert!(g.footer >= 1, "at least one rail row");
    let footer = footer_rows(&rig);
    assert!(
        compact(&footer.join("\n")).contains("Close"),
        "the rail row that is kept names the exit: {footer:?}"
    );
    assert_eq!(rig.fit().1, usize::from(g.body));
}
