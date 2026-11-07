//! K5 / M5: the actual owning view and draft survive every inspector operation.
use super::support::*;
use ratatui::style::Modifier;
use rustain::adapters::tui::{app::InputAction, keymap::Key, presentation};
use rustain::domain::{
    models::{FocusState, InvocationStatus as S, StreamChunk, Turn, WriteDiffState as D},
    services::reducer,
};
use serde_json::json;

#[test]
fn open_scroll_back_leaves_stage_and_composer_identical() {
    let (mut f, id) = Fixture::single(
        80,
        24,
        "Bash",
        json!({"command":"seq 1 99"}),
        S::Success,
        Some((&rows(99), D::NotAWrite)),
    );
    f.h.state.input_buffer = "draft text".into();
    f.h.state.cursor_position = 3;
    f.paint();
    let rect = f.layout();
    let draft = (
        f.h.state.input_buffer.clone(),
        f.h.state.cursor_position,
        f.h.state.input_scroll_offset,
    );
    let view = f.view();
    let cache = f.cache();
    let transcript = serde_json::to_vec(f.conversation()).unwrap();
    let stage = pixels(f.buffer(), rect.chat_pane);
    let composer = pixels(f.buffer(), rect.input_area);
    let mut dim_composer = composer.clone();
    for cell in &mut dim_composer {
        cell.modifier.insert(Modifier::DIM);
    }
    assert!(
        !cache.0.is_empty(),
        "the production Stage populated a real turn cache"
    );
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(id.as_str()));
    f.open();
    for key in [
        Key::Char('j'),
        Key::Ctrl('d'),
        Key::PageDown,
        Key::Right,
        Key::Down,
        Key::Right,
        Key::PageDown,
        Key::Left,
        Key::Left,
        Key::Char('G'),
        Key::Char('g'),
    ] {
        f.press(key);
        f.paint();
        let now = f.layout();
        assert_eq!(
            (now.chat_pane, now.input_area, now.status_bar),
            (rect.chat_pane, rect.input_area, rect.status_bar)
        );
        assert_eq!(
            f.view(),
            view,
            "actual Tab.view_state changed under {key:?}"
        );
        assert_eq!(
            f.cache(),
            cache,
            "actual Stage keys/entries changed under {key:?}"
        );
        assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), transcript);
        assert_eq!(
            (
                f.h.state.input_buffer.clone(),
                f.h.state.cursor_position,
                f.h.state.input_scroll_offset
            ),
            draft
        );
        assert_eq!(
            pixels(f.buffer(), now.input_area),
            dim_composer,
            "only the shared modal DIM may change the draft paint"
        );
    }
    assert_eq!(f.press(Key::Esc), InputAction::Consumed);
    f.paint();
    assert!(f.h.state.modal_stack.is_empty());
    assert!(f.h.state.tool_inspectors.is_empty());
    assert_eq!(f.h.state.focus, FocusState::Chat);
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(id.as_str()));
    assert_eq!(f.view(), view);
    assert_eq!(f.cache(), cache);
    assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), transcript);
    assert_eq!(
        (
            f.h.state.input_buffer.clone(),
            f.h.state.cursor_position,
            f.h.state.input_scroll_offset
        ),
        draft
    );
    assert_eq!(pixels(f.buffer(), rect.chat_pane), stage);
    assert_eq!(pixels(f.buffer(), rect.input_area), composer);
}

#[test]
fn escape_and_ctrl_c_back_one_frame_without_interrupting_stream_growth() {
    for close in [Key::Esc, Key::Ctrl('c')] {
        let mut turn = Turn::new("fixture-model".into(), 0);
        let id = call(
            &mut turn,
            "Bash",
            json!({"command":"streaming"}),
            S::Running,
            None,
        );
        let part = part_for(&turn, &id);
        let (mut f, _) = Fixture::single(
            80,
            24,
            "Bash",
            json!({"command":"completed"}),
            S::Success,
            Some(("done", D::NotAWrite)),
        );
        f.tabs.active_tab_mut().conversation.turns.clear();
        f.tabs
            .active_tab_mut()
            .conversation
            .rebuild_messages_mirror();
        {
            let tab = f.tabs.active_tab_mut();
            tab.reducer.open_turn = Some(turn);
            tab.reducer
                .pending_invocations
                .insert("wire-live".into(), part);
            tab.reducer
                .set_tail("wire-live", "initial-live-output".into());
            reducer::update_streaming_mirror(&tab.reducer, &mut tab.streaming);
        }
        f.h.state.input_buffer = "draft text".into();
        f.h.state.cursor_position = 4;
        f.paint();
        f.open();
        let frame = f.h.state.modal_stack.top().unwrap().clone();
        let before = f.h.state.total_content_height;
        let transcript = serde_json::to_vec(f.conversation()).unwrap();
        let draft = (f.h.state.input_buffer.clone(), f.h.state.cursor_position);
        {
            let tab = f.tabs.active_tab_mut();
            reducer::reduce(
                &mut tab.reducer,
                StreamChunk::Text {
                    content: rows(40),
                    parent_tool_use_id: None,
                },
                &*tab.clock,
            );
            tab.reducer
                .set_tail("wire-live", "updated-live-output".into());
            reducer::update_streaming_mirror(&tab.reducer, &mut tab.streaming);
        }
        f.paint();
        assert!(
            f.h.state.total_content_height > before,
            "positive control: real stream append grows the underlying Stage"
        );
        let body = (0..f.body_rect().height)
            .map(|n| f.body_row(n))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("updated-live-output"));
        assert!(!body.contains("initial-live-output"));
        assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
        assert_eq!(
            serde_json::to_vec(f.conversation()).unwrap(),
            transcript,
            "open stream is not committed by inspection"
        );
        assert_eq!(
            f.press(close),
            InputAction::Consumed,
            "{close:?} must never emit CancelOrQuit"
        );
        assert!(f.h.state.modal_stack.is_empty());
        assert!(f.h.state.tool_inspectors.is_empty());
        assert!(f.tabs.active_tab().streaming.is_streaming);
        assert!(f.tabs.active_tab().reducer.open_turn.is_some());
        assert!(!f.tabs.active_tab().turn_cancel.is_cancelled());
        assert_eq!(
            (f.h.state.input_buffer.clone(), f.h.state.cursor_position),
            draft
        );
        let still_open = f
            .tabs
            .active_tab()
            .reducer
            .open_prose
            .as_ref()
            .unwrap()
            .len();
        {
            let tab = f.tabs.active_tab_mut();
            reducer::reduce(
                &mut tab.reducer,
                StreamChunk::Text {
                    content: "\nafter-close-sentinel".into(),
                    parent_tool_use_id: None,
                },
                &*tab.clock,
            );
            reducer::update_streaming_mirror(&tab.reducer, &mut tab.streaming);
        }
        assert!(
            f.tabs
                .active_tab()
                .reducer
                .open_prose
                .as_ref()
                .unwrap()
                .len()
                > still_open
        );
        f.paint();
        assert!(presentation::consistent(&f.h.state));
    }
}
