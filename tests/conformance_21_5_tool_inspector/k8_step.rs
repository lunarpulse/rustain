//! K8 / M8: stepping is an in-place frame operation, never Stage navigation.
use super::support::*;
use rustain::adapters::tui::{keymap::Key, presentation, tool_inspector::ToolTab};
use rustain::domain::models::{InvocationStatus as S, Turn, WriteDiffState as D};
use serde_json::json;

fn fixture() -> (Fixture, [String; 3]) {
    let mut turn = Turn::new("model".into(), 0);
    let bash = call(
        &mut turn,
        "Bash",
        json!({"command":"first"}),
        S::Success,
        Some((&rows(99), D::NotAWrite)),
    );
    prose(&mut turn, &rows(22));
    let edit = call(
        &mut turn,
        "Edit",
        json!({"old_string":"old", "new_string":"new"}),
        S::Success,
        Some((&rows(99), D::NotAWrite)),
    );
    prose(&mut turn, &rows(22));
    let write = call(
        &mut turn,
        "Write",
        json!({"content":"new-file"}),
        S::Success,
        Some((&rows(99), D::NewFile)),
    );
    (Fixture::new(80, 24, turn), [bash, edit, write])
}

fn step_unpainted(f: &mut Fixture, key: Key) {
    f.step(key);
}

fn step(f: &mut Fixture, key: Key) {
    step_unpainted(f, key);
    f.paint();
}

fn title(f: &Fixture) -> &str {
    f.h.state.modal_stack.top().unwrap().title()
}

fn rail(f: &Fixture) -> String {
    let parts = rustain::adapters::tui::tool_inspector::layout::parts(f.frame_rect());
    row(f.buffer(), parts.rail, parts.rail.y)
}

#[test]
fn brackets_never_leave_the_turn() {
    let (mut f, ids) = fixture();
    let mut other = Turn::new("other-model".into(), 0);
    call(
        &mut other,
        "Read",
        json!({"file_path":"other-turn"}),
        S::Success,
        Some(("forbidden-other-turn", D::NotAWrite)),
    );
    f.tabs.active_tab_mut().conversation.turns.push(other);
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.focus_call(&ids[2]);
    f.open();
    for _ in 0..3 {
        f.press(Key::Right);
    }
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Diff);
    let frame_id = f.h.state.modal_stack.top().unwrap().id;
    let stage_focus = f.h.state.focused_tool_id.clone();
    let view = f.view();
    let cache = f.cache();
    assert!(!rail(&f).contains("Next"));
    step(&mut f, Key::Char(']'));
    assert_eq!(title(&f), "Tool · Write #3");
    step(&mut f, Key::Char('['));
    assert_eq!(title(&f), "Tool · Edit #2");
    assert_eq!(f.tool().tab, ToolTab::Diff, "applicable tab is retained");
    assert!(f.body_row(0).contains("- old"));
    assert!(rail(&f).contains("Previous") && rail(&f).contains("Next"));
    step(&mut f, Key::Char('['));
    assert_eq!(title(&f), "Tool · Bash #1");
    assert_eq!(
        f.tool().tab,
        ToolTab::Output,
        "unavailable Diff falls back to Output"
    );
    assert!(f.body_row(0).contains("row 000"));
    assert!(!rail(&f).contains("Previous"));
    step(&mut f, Key::Char('['));
    assert_eq!(title(&f), "Tool · Bash #1");
    assert_eq!(f.h.state.modal_stack.top().unwrap().id, frame_id);
    assert_eq!(f.h.state.focused_tool_id, stage_focus);
    assert_eq!(f.view(), view);
    assert_eq!(f.cache(), cache);
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.h.state.focused_tool_id, stage_focus);
}

#[test]
fn step_resets_viewers_and_prepaint_input_targets_the_new_body() {
    let (mut f, ids) = fixture();
    f.focus_call(&ids[0]);
    f.open();
    f.press(Key::Char('G'));
    f.paint();
    assert!(
        f.tool().viewers[0].top > 0,
        "observable scrolled-source control"
    );
    step(&mut f, Key::Char(']'));
    assert_eq!(title(&f), "Tool · Edit #2");
    assert_eq!(f.tool().viewers[0].top, 0);
    assert!(f.body_row(0).contains("row 000"));
    // Return to Bash without a paint between the step and its navigation.
    step_unpainted(&mut f, Key::Char('['));
    f.press(Key::Char('G'));
    f.press(Key::Char('k'));
    f.paint();
    let maximum = f.tool().rows().len().saturating_sub(f.tool().viewport);
    assert_eq!(f.tool().viewers[0].top, maximum - 1);
}

#[test]
fn stepping_finds_new_invocations_in_the_owning_open_turn() {
    let (mut f, _) = Fixture::single(80, 24, "Bash", json!({}), S::Success, None);
    let mut open = Turn::new("streaming-model".into(), 0);
    let first = call(
        &mut open,
        "Bash",
        json!({"command":"first"}),
        S::Running,
        None,
    );
    call(
        &mut open,
        "Edit",
        json!({"old_string":"old","new_string":"new"}),
        S::Running,
        None,
    );
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.tabs.active_tab_mut().reducer.open_turn = Some(open);
    {
        let tab = f.tabs.active_tab_mut();
        rustain::domain::services::reducer::update_streaming_mirror(
            &tab.reducer,
            &mut tab.streaming,
        );
    }
    f.focus_call(&first);
    f.open();
    step(&mut f, Key::Char(']'));
    assert_eq!(title(&f), "Tool · Edit #2");
    call(
        f.tabs.active_tab_mut().reducer.open_turn.as_mut().unwrap(),
        "Write",
        json!({"content":"third"}),
        S::Running,
        None,
    );
    step(&mut f, Key::Char(']'));
    assert_eq!(title(&f), "Tool · Write #3");
    assert!(f.h.screen_text().contains("Following"));
}

#[test]
fn two_frame_collision_keeps_payloads_independent_and_back_restores_lower() {
    let (mut f, ids) = fixture();
    f.focus_call(&ids[0]);
    f.open();
    f.press(Key::Ctrl('d'));
    f.paint();
    let lower = f.h.state.modal_stack.top().unwrap().id;
    let lower_cache = f.tool().heights.message_entries.clone();
    let lower_top = f.tool().viewers[0].top;
    f.focus_call(&ids[1]);
    assert!(
        f.inspect(),
        "same production opening seam used by existing multi-instance witness"
    );
    f.paint();
    let upper = f.h.state.modal_stack.top().unwrap().id;
    assert_ne!(lower, upper);
    step(&mut f, Key::Char('['));
    assert_eq!(title(&f), "Tool · Bash #1");
    assert_eq!(
        f.h.state.tool_inspectors[&lower].target,
        f.h.state.tool_inspectors[&upper].target
    );
    assert_eq!(f.h.state.modal_stack.top().unwrap().id, upper);
    assert!(presentation::consistent(&f.h.state));
    f.press(Key::Char('j'));
    f.paint();
    assert_eq!(f.h.state.tool_inspectors[&lower].viewers[0].top, lower_top);
    assert_eq!(
        f.h.state.tool_inspectors[&lower].heights.message_entries,
        lower_cache
    );
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.h.state.modal_stack.top().unwrap().id, lower);
    assert_eq!(f.tool().viewers[0].top, lower_top);
    assert_eq!(f.tool().heights.message_entries, lower_cache);
    assert!(!f.h.state.tool_inspectors.contains_key(&upper));
    f.refresh();
    assert!(
        !f.h.state.tool_inspectors.contains_key(&upper),
        "late refresh cannot revive released frame"
    );
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn exact_target_reopen_after_a_step_collision_truncates_to_the_lower_owner() {
    let (mut f, ids) = fixture();
    f.focus_call(&ids[0]);
    f.open();
    f.press(Key::Char('j'));
    f.paint();
    let lower = f.h.state.modal_stack.top().unwrap().id;
    let before = format!("{:?}", f.tool());
    f.focus_call(&ids[1]);
    assert!(f.inspect());
    f.paint();
    step(&mut f, Key::Char('['));
    f.focus_call(&ids[0]);
    assert!(f.inspect());
    f.paint();
    assert_eq!(f.h.state.modal_stack.len(), 1);
    assert_eq!(f.h.state.modal_stack.top().unwrap().id, lower);
    assert_eq!(format!("{:?}", f.tool()), before);
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn live_collision_refreshes_each_owner_and_late_completion_never_revives_a_closed_frame() {
    use rustain::domain::{
        models::{StreamChunk, TurnPart, tool_call_id_for},
        services::reducer,
    };
    for close_before_completion in [false, true] {
        let mut f = Fixture::new(80, 24, Turn::new("streaming-model".into(), 0));
        let initial = f.tabs.active_tab_mut().conversation.turns.pop().unwrap();
        f.tabs.active_tab_mut().reducer.open_turn = Some(initial);
        f.tabs
            .active_tab_mut()
            .conversation
            .rebuild_messages_mirror();
        for (wire, name) in [("shared-bash", "Bash"), ("shared-edit", "Edit")] {
            reducer::reduce(
                &mut f.tabs.active_tab_mut().reducer,
                StreamChunk::ToolUse {
                    id: wire.into(),
                    name: name.into(),
                    input: json!({}),
                },
                &*f.clock,
            );
            if wire == "shared-bash" {
                reducer::reduce(
                    &mut f.tabs.active_tab_mut().reducer,
                    StreamChunk::Text {
                        content: rows(22).replace('\n', "\n\n"),
                        parent_tool_use_id: None,
                    },
                    &*f.clock,
                );
            }
        }
        {
            let tab = f.tabs.active_tab_mut();
            tab.reducer.set_tail("shared-bash", rows(16));
            reducer::update_streaming_mirror(&tab.reducer, &mut tab.streaming);
        }
        // Paint both Stage headers from a committed snapshot of the still-open
        // turn. The fresher owning reducer must remain authoritative for the
        // inspector, including the subsequent real completion.
        let snapshot = f
            .tabs
            .active_tab()
            .reducer
            .open_turn
            .as_ref()
            .unwrap()
            .clone();
        f.tabs.active_tab_mut().conversation.turns.push(snapshot);
        f.tabs
            .active_tab_mut()
            .conversation
            .rebuild_messages_mirror();
        // This snapshot is already on Stage; do not paint the same turn twice.
        f.tabs.active_tab_mut().streaming = Default::default();
        let turn = f.tabs.active_tab().reducer.open_turn.as_ref().unwrap();
        let ids: Vec<_> = turn
            .parts
            .iter()
            .filter_map(|part| match part {
                TurnPart::ToolInvocation { id, .. } => Some(tool_call_id_for(&turn.id, *id)),
                _ => None,
            })
            .collect();
        // Publish the initial owning-tab geometry before scanning header offsets.
        f.paint();
        f.focus_call(&ids[0]);
        f.open();
        f.press(Key::Char('g'));
        f.paint();
        let lower = f.h.state.modal_stack.top().unwrap().id;
        assert!(!f.tool().viewers[0].follow);
        f.focus_call(&ids[1]);
        assert!(f.inspect());
        f.paint();
        let upper = f.h.state.modal_stack.top().unwrap().id;
        step(&mut f, Key::Char('['));
        assert!(f.tool().viewers[0].follow);
        assert!(
            f.tool().viewers[0].top > 0,
            "following upper differs observably from paused lower"
        );
        if close_before_completion {
            f.press(Key::Esc);
            f.paint();
            assert!(!f.h.state.tool_inspectors.contains_key(&upper));
        }
        reducer::reduce(
            &mut f.tabs.active_tab_mut().reducer,
            StreamChunk::ToolResult {
                id: "shared-bash".into(),
                operator_private: false,
                content: rows(60),
                is_error: false,
                diff: D::NotAWrite,
                images: vec![],
            },
            &*f.clock,
        );
        f.paint();
        let lower_payload = &f.h.state.tool_inspectors[&lower];
        assert_eq!(
            lower_payload.status,
            S::Success,
            "real canonical completion is observed"
        );
        assert!(lower_payload.rows().len() >= 60);
        assert_eq!(lower_payload.viewers[0].top, 0);
        if !close_before_completion {
            let upper_payload = &f.h.state.tool_inspectors[&upper];
            assert_eq!(upper_payload.status, S::Success);
            assert_eq!(
                upper_payload.viewers[0].top,
                upper_payload
                    .rows()
                    .len()
                    .saturating_sub(upper_payload.viewport)
            );
            let lower_cache = lower_payload.heights.message_entries.clone();
            f.press(Key::Esc);
            f.paint();
            assert_eq!(f.tool().heights.message_entries, lower_cache);
        }
        assert_eq!(f.h.state.modal_stack.top().unwrap().id, lower);
        assert_eq!(f.tool().viewers[0].top, 0);
        assert!(f.body_row(0).contains("row 000"));
        assert!(!f.h.state.tool_inspectors.contains_key(&upper));
        f.refresh();
        assert!(!f.h.state.tool_inspectors.contains_key(&upper));
        assert!(presentation::consistent(&f.h.state));
    }
}

#[test]
fn owning_manager_reconciliation_discards_both_colliding_frame_ids() {
    use rustain::domain::models::Conversation;
    let (mut f, ids) = fixture();
    f.focus_call(&ids[0]);
    f.open();
    let lower = f.h.state.modal_stack.top().unwrap().id;
    f.focus_call(&ids[1]);
    assert!(f.inspect());
    f.paint();
    step(&mut f, Key::Char('['));
    let upper = f.h.state.modal_stack.top().unwrap().id;
    assert_ne!(lower, upper);
    assert_eq!(
        f.h.state.tool_inspectors[&lower].target,
        f.h.state.tool_inspectors[&upper].target
    );
    assert_eq!(f.h.state.tool_inspectors.len(), 2);
    let original = f.conversation().id.clone();
    f.tabs.create_tab_with_conversation(Conversation {
        id: "independent-destination".into(),
        ..Conversation::default()
    });
    f.tabs.active_tab_mut().view_state.scroll_offset = 7;
    let destination_view = f.view();
    f.load();
    assert_ne!(f.h.conversation.id, original);
    assert_eq!(
        f.view(),
        destination_view,
        "discard cannot restore a source view into its destination"
    );
    assert!(f.h.state.modal_stack.is_empty());
    assert!(
        f.h.state.tool_inspectors.is_empty(),
        "both frame ids, not only one target-keyed entry, release"
    );
    f.refresh();
    assert!(f.h.state.tool_inspectors.is_empty());
    assert!(presentation::consistent(&f.h.state));
}
