//! Stage witnesses share the manager that owns the rendered conversation and view.
use crate::e2e_harness::TestHarness;
use rustain::domain::models::{AnchorMode, FocusState, TabManager};
use tokio_util::sync::CancellationToken;

pub fn stage(width: u16, height: u16) -> (TestHarness, TabManager) {
    let mut h = TestHarness::with_size(width, height);
    for message in 0..3 {
        h.send_message(
            &(0..24)
                .map(|row| format!("stage-{message}-row-{row:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let mut tabs = TabManager::new(CancellationToken::new());
    tabs.active_tab_mut().conversation = std::mem::take(&mut h.conversation);
    tabs.active_tab_mut().session_meta.bookmarks = vec![0, 2];
    tabs.active_tab_mut().view_state.scroll_offset = 3;
    tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
    h.state.active_tab_id = tabs.active_tab_id();
    h.state.focus = FocusState::Chat;
    h.render_active_tab(&tabs);
    (h, tabs)
}

pub fn view(tabs: &TabManager) -> String {
    format!("{:?}", tabs.active_tab().view_state)
}

pub fn chat_pixels(h: &TestHarness) -> String {
    let rect = super::k3_compose::layout(&h.state).chat_pane;
    let buffer = h.terminal.backend().buffer();
    (rect.y..rect.bottom())
        .flat_map(|y| (rect.x..rect.right()).map(move |x| buffer[(x, y)].symbol()))
        .collect()
}

pub fn cache_keys(h: &TestHarness, tabs: &TabManager) -> (Vec<String>, Vec<String>) {
    let render = &h.state.tab_render_states[&tabs.active_tab_id()];
    let mut turns = render
        .height_cache
        .entries
        .iter()
        .map(|(key, entry)| format!("{key:?}={entry:?}"))
        .collect::<Vec<_>>();
    let mut messages = render
        .height_cache
        .message_entries
        .iter()
        .map(|(key, height)| format!("{key:?}={height}"))
        .collect::<Vec<_>>();
    turns.sort();
    messages.sort();
    (turns, messages)
}

#[test]
fn cache_snapshot_observes_actual_stage_entry_changes() {
    let (mut h, tabs) = stage(80, 24);
    let before = cache_keys(&h, &tabs);
    let render = h
        .state
        .tab_render_states
        .get_mut(&tabs.active_tab_id())
        .unwrap();
    let height = render
        .height_cache
        .message_entries
        .values_mut()
        .next()
        .expect("production Stage rendering populated a message entry");
    *height += 1;
    assert_ne!(
        cache_keys(&h, &tabs),
        before,
        "entry-height mutations must be visible even with identical cache keys"
    );
}

pub fn paste(
    h: &mut TestHarness,
    tabs: &mut TabManager,
    text: &str,
) -> rustain::adapters::tui::app::InputAction {
    let conversation = tabs.active_tab().conversation.clone();
    rustain::adapters::tui::handlers::search::handle_query_paste(
        tabs,
        &mut h.state,
        &conversation,
        text,
    )
    .expect("the actual field owner handles borrowed paste")
}
