//! K6 (AC7): the frame owns its payload; the Stage, its caches and the composer
//! are untouched by opening, navigating, searching and closing S-BLOCK.
use rustain::adapters::tui::{
    app::InputAction,
    keymap::Key,
    presentation::{self, Surface},
};
use rustain::domain::models::Turn;

use super::support::{Fixture, block, code, focus, mixed_turn, open_focused, part};

/// Everything the Stage and composer own: both height-cache halves, the owning
/// ViewState, the draft and the chat/input rectangles.
fn stage(f: &Fixture) -> String {
    let s = &f.h.state;
    format!(
        "{} {:?} {:?} {:?} {:?}",
        f.view(),
        f.cache(),
        (&s.input_buffer, s.cursor_position, s.input_scroll_offset),
        (&s.focus, &s.focused_item_id),
        (f.layout().chat_pane, f.layout().input_area),
    )
}

fn keys(f: &mut Fixture, keys: &[Key]) {
    for key in keys {
        f.press(*key);
    }
    f.compose();
}

#[test]
fn opening_scrolling_and_closing_the_viewer_leave_the_stage_untouched() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    f.h.state.input_buffer = "retained draft".into();
    f.h.state.cursor_position = 4;
    focus(&mut f, &ids[0]);
    f.paint();
    let before = stage(&f);
    open_focused(&mut f);
    assert_eq!(
        stage(&f),
        before,
        "opening must not edit the Stage or draft"
    );
    let mut chars: Vec<Key> = vec![Key::Char('j'); 5];
    chars.extend([Key::Char('$'), Key::Char('/'), Key::Char('x'), Key::Enter]);
    keys(&mut f, &chars);
    assert_eq!(
        stage(&f),
        before,
        "scrolling and searching stay inside the frame"
    );
    assert!(
        block(&f).viewer.h_off > 0,
        "positive control: the frame did move"
    );
    keys(&mut f, &[Key::Esc]);
    f.paint();
    assert!(f.h.state.modal_stack.is_empty() && f.h.state.block_viewers.is_empty());
    assert_eq!(stage(&f), before);
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(ids[0].as_str()));
}

#[test]
fn escape_and_ctrl_c_back_one_frame_to_the_same_focused_item() {
    for close in [Key::Esc, Key::Ctrl('c')] {
        let (turn, ids) = mixed_turn();
        let mut f = Fixture::new(80, 24, turn);
        focus(&mut f, &ids[2]);
        open_focused(&mut f);
        assert_eq!(f.h.state.modal_stack.len(), 1);
        assert_eq!(f.press(close), InputAction::Consumed, "{close:?}");
        assert!(f.h.state.modal_stack.is_empty());
        assert!(f.h.state.block_viewers.is_empty());
        f.paint();
        assert_eq!(f.h.state.focused_item_id.as_deref(), Some(ids[2].as_str()));
        assert!(!f.tabs.active_tab().turn_cancel.is_cancelled());
        assert!(presentation::consistent(&f.h.state));
    }
}

#[test]
fn a_stage_replacement_pops_the_stage_bound_frame() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    focus(&mut f, &ids[0]);
    open_focused(&mut f);
    assert!(matches!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::BlockViewer(_)
    ));
    rustain::infrastructure::runtime::event_loop::replace_stage_conversation(
        &mut f.h.conversation,
        &mut f.h.state,
        rustain::domain::models::Conversation::default(),
    );
    assert!(f.h.state.modal_stack.is_empty() && f.h.state.block_viewers.is_empty());
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn a_stream_appending_under_the_viewer_keeps_growing_the_stage() {
    let (mut f, id) = super::support::single(80, 24, &code(5));
    focus(&mut f, &id);
    open_focused(&mut f);
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    let height = f.h.state.total_content_height;
    let mut later = Turn::new("fixture-model".into(), 1);
    part(&mut later, &"Later paragraph.\n\n".repeat(30));
    f.tabs.active_tab_mut().conversation.turns.push(later);
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.paint();
    assert!(
        f.h.state.total_content_height > height,
        "positive control: the Stage grew"
    );
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert!(!block(&f).missing);
}

#[test]
fn opening_leaves_every_stage_cache_entry_in_place_before_any_repaint() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    focus(&mut f, &ids[0]);
    f.paint();
    let before = f.cache();
    assert!(
        !before.0.is_empty(),
        "control: the Stage cache is populated"
    );
    assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
    let tab = f.tabs.active_tab();
    assert!(rustain::adapters::tui::block_viewer::inspect_focused(
        &mut f.h.state,
        &tab.conversation,
        tab.reducer.open_turn.as_ref(),
    ));
    assert_eq!(
        f.cache(),
        before,
        "the host effect itself must not invalidate the Stage"
    );
}
