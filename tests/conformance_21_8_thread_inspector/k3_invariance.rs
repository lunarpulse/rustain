//! K3 (local half, AC5): the frame owns its payload. Opening, scrolling, paging and
//! closing the thread leave the Stage (chat rect, ViewState, height caches, focused
//! block), the composer (text, caret, scroll) and the base focus exactly as they were;
//! the Stage underneath keeps growing; a decision resolved over the frame never leaves
//! the base focus on the covered Stage.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{self, INERT_AFTER_PAINT};
use rustain::adapters::tui::keymap::Key;
use rustain::domain::models::{
    FocusState, ImageAttachment, ReasoningDisplay, StreamChunk, Turn, TurnPart,
    turn::block_item_id_for,
};
use rustain::domain::services::reducer;
use std::time::Duration;

use super::support::*;
use crate::conformance_21_13_block_viewer::support::{code, focus, part};

/// `[folded reasoning row, prose with a 12-line code block]`: the code block is the
/// Stage's focusable item.
fn stage_turn() -> (Turn, String) {
    let mut turn = Turn::new("fixture-model".into(), 0);
    turn.push_part(|id| TurnPart::Reasoning {
        id,
        text: "weighing the options carefully".into(),
        duration_ms: Some(4_200),
        tokens: Some(1_300),
    });
    let code_part = part(&mut turn, &format!("Intro paragraph.\n\n{}", code(12)));
    let id = block_item_id_for(&turn.id, code_part, 0);
    (turn, id)
}

async fn stage_rig(ws: Workspace) -> (Rig, String) {
    let (turn, code_id) = stage_turn();
    let mut rig = Rig::new(ws, 130, 30, turn).await;
    rig.f.h.state.reasoning_display = ReasoningDisplay::Folded;
    rig.f.h.state.input_buffer = "draft text".into();
    rig.f.h.state.cursor_position = 3;
    focus(&mut rig.f, &code_id);
    rig.paint();
    assert_eq!(
        rig.f.h.state.focused_item_id.as_deref(),
        Some(code_id.as_str()),
        "the code block is the focused Stage item"
    );
    assert!(
        rig.f.h.screen_text().contains("Thinking"),
        "positive control: the folded reasoning row is on the Stage"
    );
    (rig, code_id)
}

/// Everything the Stage and composer own.
fn snap(rig: &Rig) -> String {
    let s = &rig.f.h.state;
    format!(
        "{:?} {} {:?} {:?} {:?}",
        rig.f.layout().chat_pane,
        rig.f.view(),
        rig.f.cache(),
        (&s.input_buffer, s.cursor_position, s.input_scroll_offset),
        (&s.focus, &s.focused_item_id),
    )
}

async fn invariance(ws: Workspace, id: &str, paged: bool) {
    let (mut rig, _code_id) = stage_rig(ws).await;
    let before = snap(&rig);
    let layout = rig.f.layout();
    let stage = pixels(rig.f.buffer(), layout.chat_pane);
    let composer = pixels(rig.f.buffer(), layout.input_area);

    rig.view(id).await;
    rig.paint();
    assert!(rig.is_open());
    assert_eq!(
        snap(&rig),
        before,
        "opening wrote nothing the Stage or composer owns"
    );

    rig.keys(&[Key::PageDown, Key::PageDown, Key::PageDown]);
    rig.paint();
    if paged {
        assert!(
            rig.viewer_top() > 0,
            "positive control: PgDn scrolled the window"
        );
    }
    assert_eq!(snap(&rig), before, "scrolling stays inside the frame");

    let action = rig.key(Key::Char('p'));
    if paged {
        assert_eq!(action, InputAction::ThreadPage { earlier: true });
        let first = rig.inspector().first_non_pinned_ordinal;
        rig.navigate(true).await;
        assert_ne!(
            rig.inspector().first_non_pinned_ordinal,
            first,
            "positive control: the earlier page loaded"
        );
    } else {
        assert!(matches!(
            action,
            InputAction::Consumed | InputAction::Ignored
        ));
        assert_eq!(rig.inspector().omitted_before, 0, "no earlier page to load");
    }
    rig.paint();
    assert_eq!(snap(&rig), before, "paging stays inside the frame");

    rig.key(Key::Esc);
    rig.paint();
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert!(rig.f.h.state.thread_inspector.is_none());
    assert_eq!(
        snap(&rig),
        before,
        "closing restores exactly what was covered"
    );
    let layout = rig.f.layout();
    assert_eq!(
        pixels(rig.f.buffer(), layout.chat_pane),
        stage,
        "Stage cells"
    );
    assert_eq!(
        pixels(rig.f.buffer(), layout.input_area),
        composer,
        "composer cells"
    );
}

#[tokio::test]
async fn k3_open_scroll_close_leave_stage_composer_and_focus_identical() {
    invariance(consultation_workspace().await, "ri_c", false).await;
}

#[tokio::test]
async fn k3_a_paged_thread_closes_to_the_identical_stage() {
    invariance(long_workspace().await, NEWEST, true).await;
}

#[tokio::test]
async fn k3_a_streamed_chunk_under_the_frame_grows_the_stage_and_never_moves_the_frame() {
    let (mut rig, _) = stage_rig(consultation_workspace().await).await;
    rig.view("ri_c").await;
    rig.paint();
    let frame = rig.f.h.state.modal_stack.top().unwrap().clone();
    let viewer = rig.inspector().viewer;
    let composer = (
        rig.f.h.state.input_buffer.clone(),
        rig.f.h.state.cursor_position,
        rig.f.h.state.input_scroll_offset,
    );
    let before = rig.f.h.state.total_content_height;
    {
        let tab = rig.f.tabs.active_tab_mut();
        tab.reducer.open_turn = Some(Turn::new("fixture-model".into(), 1));
        reducer::reduce(
            &mut tab.reducer,
            StreamChunk::Text {
                content: rows(40),
                parent_tool_use_id: None,
            },
            &*tab.clock,
        );
        reducer::update_streaming_mirror(&tab.reducer, &mut tab.streaming);
    }
    rig.paint();
    assert!(
        rig.f.h.state.total_content_height > before,
        "positive control: the real stream append grew the Stage under the frame"
    );
    assert_eq!(
        rig.f.h.state.modal_stack.top(),
        Some(&frame),
        "the frame is untouched"
    );
    assert_eq!(rig.inspector().viewer, viewer);
    assert_eq!(
        (
            rig.f.h.state.input_buffer.clone(),
            rig.f.h.state.cursor_position,
            rig.f.h.state.input_scroll_offset
        ),
        composer
    );
    rig.key(Key::Esc);
    rig.paint();
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert!(
        rig.f.h.state.total_content_height > before,
        "the grown Stage is what the close reveals"
    );
}

#[tokio::test]
async fn k3_a_large_image_resolved_over_the_frame_leaves_base_focus_on_the_composer() {
    let mut rig = Rig::plain(consultation_workspace().await, 100, 30).await;
    assert_eq!(
        rig.f.h.state.focus,
        FocusState::Chat,
        "the Stage owned focus"
    );
    rig.view("ri_c").await;
    rig.paint();
    assert!(rig.is_open());
    decision_queue::raise_large_image(
        &mut rig.f.h.state,
        ImageAttachment {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        },
        "Image is large (6 MB).",
    );
    assert_eq!(rig.depth(), 2, "the decision opened over the thread");
    rig.paint();
    rig.f
        .clock
        .advance(INERT_AFTER_PAINT + Duration::from_millis(1));
    let action = rig.key(Key::Char('y'));
    assert_eq!(action, InputAction::ImageConfirmAttach);
    decision_queue::apply_resolved_action(&mut rig.f.h.state, action);
    assert_eq!(
        rig.f.h.state.pending_images.len(),
        1,
        "attached exactly once"
    );
    // The frame stays on the stack, so the resolve writes no base focus at all (a retained
    // frame is never the source of a base pick, `decision_queue::apply_resolved_action`).
    assert_eq!(
        rig.f.h.state.focus,
        FocusState::Chat,
        "the frame never owns base focus: the resolve leaves the base untouched"
    );
    assert!(rig.is_open(), "the interrupted thread is back");
    rig.key(Key::Esc);
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert_eq!(
        rig.f.h.state.focus,
        FocusState::Chat,
        "Esc restores exactly the pre-open base"
    );
    // Positive control: with an empty stack the same resolve picks the composer.
    decision_queue::apply_resolved_action(&mut rig.f.h.state, InputAction::ImageConfirmCancel);
    assert_eq!(rig.f.h.state.focus, FocusState::Input);
}

/// An empty conversation paints the launchpad as its Stage. Opening, scrolling, paging and
/// closing the thread frame over it leaves those Stage cells byte-identical.
#[tokio::test]
async fn k3_an_empty_conversation_keeps_its_launchpad_rows_identical_across_open_page_close() {
    let mut rig = Rig::plain(long_workspace().await, 130, 30).await;
    {
        let tab = rig.f.tabs.active_tab_mut();
        tab.conversation.turns.clear();
        tab.conversation.rebuild_messages_mirror();
    }
    rig.f.h.state.launchpad_env = Some(rustain::adapters::tui::launchpad::LaunchpadEnv::new(
        rig.ws.path().to_path_buf(),
        |_| false,
    ));
    rig.paint();
    assert!(
        rig.f.h.screen_text().contains("Welcome to Rustain."),
        "positive control: the empty conversation paints the launchpad: {}",
        rig.f.h.screen_text()
    );
    let layout = rig.f.layout();
    let stage = pixels(rig.f.buffer(), layout.chat_pane);
    let composer = pixels(rig.f.buffer(), layout.input_area);
    let before = snap(&rig);

    rig.view(NEWEST).await;
    rig.paint();
    assert!(rig.is_open());
    assert!(
        title_row(&rig.f).contains("Thread ·"),
        "positive control: the frame is painted over the launchpad"
    );
    assert_eq!(snap(&rig), before, "opening wrote nothing the Stage owns");
    rig.keys(&[Key::PageDown, Key::PageDown]);
    rig.paint();
    assert!(
        rig.viewer_top() > 0,
        "positive control: the window scrolled"
    );
    assert_eq!(
        rig.key(Key::Char('p')),
        InputAction::ThreadPage { earlier: true }
    );
    rig.navigate(true).await;
    rig.paint();
    assert_eq!(snap(&rig), before, "paging stays inside the frame");

    rig.key(Key::Esc);
    rig.paint();
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert_eq!(snap(&rig), before, "closing restores what was covered");
    let layout = rig.f.layout();
    assert_eq!(
        pixels(rig.f.buffer(), layout.chat_pane),
        stage,
        "the launchpad cells are byte-identical after close"
    );
    assert_eq!(pixels(rig.f.buffer(), layout.input_area), composer);
    assert!(rig.f.h.screen_text().contains("Welcome to Rustain."));
}
