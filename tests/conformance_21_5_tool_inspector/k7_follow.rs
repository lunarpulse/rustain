//! K7 / M6 / M7: canonical reducer growth through the owning host renderer.
use super::support::*;
use rustain::adapters::tui::{app::InputAction, keymap::Key, tool_inspector::ToolTab};
use rustain::domain::{
    models::{InvocationStatus as S, StreamChunk, TurnPart, WriteDiffState as D},
    services::reducer,
};
use serde_json::json;

fn running(count: usize) -> Fixture {
    let (mut f, _) = Fixture::single(80, 22, "Bash", json!({}), S::Success, None);
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    reducer::reduce(
        &mut f.tabs.active_tab_mut().reducer,
        StreamChunk::ToolUse {
            id: "wire-follow".into(),
            name: "Bash".into(),
            input: json!({"command":"stream"}),
        },
        &*f.clock,
    );
    set_tail(&mut f, count);
    f.paint();
    assert!(
        f.h.state.focused_tool_id.is_some(),
        "real Stage seats the running call"
    );
    f
}

fn set_tail(f: &mut Fixture, count: usize) {
    let tab = f.tabs.active_tab_mut();
    tab.reducer.set_tail("wire-follow", rows(count));
    reducer::update_streaming_mirror(&tab.reducer, &mut tab.streaming);
}

fn complete(f: &mut Fixture) {
    reducer::reduce(
        &mut f.tabs.active_tab_mut().reducer,
        StreamChunk::ToolResult {
            id: "wire-follow".into(),
            operator_private: false,
            content: rows(60),
            is_error: false,
            diff: D::NotAWrite,
            images: vec![],
        },
        &*f.clock,
    );
}

fn maximum(f: &Fixture) -> usize {
    f.tool().rows().len().saturating_sub(f.tool().viewport)
}

fn status(f: &Fixture) -> String {
    let rect = rustain::adapters::tui::tool_inspector::layout::parts(f.frame_rect())
        .status
        .expect("80x22 has room for the viewer status");
    row(f.buffer(), rect, rect.y)
}

fn shows(f: &Fixture, wanted: &str) -> bool {
    (0..f.body_rect().height).any(|offset| f.body_row(offset).contains(wanted))
}

#[test]
fn running_tool_opens_following_the_tail() {
    for invocation_status in [S::Pending, S::Running] {
        let mut f = running(16);
        let id = f.h.state.focused_tool_id.clone().unwrap();
        if let TurnPart::ToolInvocation { status, .. } = f.invocation_mut(&id) {
            *status = invocation_status;
        }
        f.open();
        assert!(maximum(&f) > 0, "control: the tail exceeds the fitted body");
        assert_eq!(f.tool().viewers[ToolTab::Output.index()].top, maximum(&f));
        assert!(
            shows(&f, "row 015"),
            "the final source line must be painted"
        );
        assert!(status(&f).contains("Following"));
    }
}

#[test]
fn paused_view_keeps_its_row_when_output_grows() {
    let mut f = running(12);
    f.open();
    f.press(Key::Char('k'));
    f.paint();
    let top = f.tool().viewers[0].top;
    let first = f.body_row(0);
    assert!(status(&f).contains("Paused"));
    set_tail(&mut f, 16);
    f.paint();
    assert!(
        maximum(&f) > top,
        "positive control: growth extends the scroll range"
    );
    assert_eq!(f.tool().viewers[0].top, top);
    assert_eq!(f.body_row(0), first);
    assert!(status(&f).contains("Paused"));
}

#[test]
fn growth_and_completion_follow_only_until_manual_navigation() {
    for pause in [false, true] {
        let mut f = running(12);
        f.open();
        if pause {
            f.press(Key::Char('k'));
            f.paint();
        }
        let previous = f.tool().viewers[0].top;
        set_tail(&mut f, 16);
        f.paint();
        assert_eq!(
            f.tool().viewers[0].top,
            if pause { previous } else { maximum(&f) }
        );
        complete(&mut f);
        f.paint();
        assert_eq!(f.tool().status, S::Success);
        assert_eq!(
            f.tool().viewers[0].top,
            if pause { previous } else { maximum(&f) }
        );
        assert!(!status(&f).contains("Following") && !status(&f).contains("Paused"));
        if !pause {
            assert!(shows(&f, "row 059"));
        }
    }
}

#[test]
fn g_resumes_following_after_a_pause_and_tab_switch() {
    let mut f = running(12);
    f.open();
    f.press(Key::Char('g'));
    f.paint();
    assert!(status(&f).contains("Paused"));
    f.press(Key::Right);
    f.paint();
    set_tail(&mut f, 16);
    f.paint();
    f.press(Key::Left);
    f.paint();
    assert_eq!(
        f.tool().viewers[0].top,
        0,
        "paused tab does not follow while covered"
    );
    f.press(Key::Char('G'));
    f.paint();
    assert!(status(&f).contains("Following"));
    complete(&mut f);
    f.paint();
    assert!(shows(&f, "row 059"));
}

#[test]
fn no_tail_and_completed_opening_report_honest_states() {
    let mut f = running(0);
    f.open();
    assert!(shows(&f, "No output yet — running"));
    assert!(status(&f).contains("Following"));
    f.press(Key::Esc);
    complete(&mut f);
    f.paint();
    f.open();
    assert_eq!(f.tool().viewers[0].top, 0);
    assert!(f.body_row(0).contains("row 000"));
    assert!(!status(&f).contains("Following") && !status(&f).contains("Paused"));
}

#[test]
fn prepaint_manual_navigation_pauses_the_initial_tail() {
    let mut f = running(16);
    assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
    assert!(f.inspect());
    f.press(Key::Char('k'));
    f.paint();
    assert_eq!(f.tool().viewers[0].top, maximum(&f) - 1);
    assert!(status(&f).contains("Paused"));
    let top = f.tool().viewers[0].top;
    complete(&mut f);
    f.paint();
    assert_eq!(f.tool().viewers[0].top, top);
}
