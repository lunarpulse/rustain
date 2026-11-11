//! K10 / M9: the real Tab decoder reaches the owning-manager cycle effect.
//! The host-only result installation below does not implement turn resolution
//! or scrolling: both must come from production `cycle_item`.
use super::support::*;
use rustain::adapters::tui::{
    app::InputAction,
    handlers::tool_focus::{CycleOutcome, cycle_item},
    keymap::Key,
    state::HeightKey,
    widgets::chat_pane,
};
use rustain::domain::models::{
    AnchorMode, FocusState, InvocationStatus as S, StopReason, Turn, TurnId, WriteDiffState as D,
};
use serde_json::json;

fn three(spaced: bool) -> (Fixture, [String; 3]) {
    let mut turn = Turn::new("k10-model".into(), 0);
    let error = rows(60);
    let first = call(
        &mut turn,
        "Bash",
        json!({"command":"k10-first"}),
        if spaced { S::Error } else { S::Success },
        Some((if spaced { &error } else { "FIRST-OUTPUT" }, D::NotAWrite)),
    );
    let second = call(
        &mut turn,
        "Edit",
        json!({"file_path":"k10-second", "old_string":"old", "new_string":"new"}),
        S::Success,
        Some(("SECOND-OUTPUT", D::NotAWrite)),
    );
    if spaced {
        prose(&mut turn, &rows(18));
    }
    let third = call(
        &mut turn,
        "Write",
        json!({"file_path":"k10-third", "content":"new"}),
        S::Success,
        Some(("THIRD-OUTPUT", D::NewFile)),
    );
    // Leave enough real content after the last header to align it to the top.
    if spaced {
        prose(&mut turn, &rows(20));
    }
    turn.stop_reason = Some(StopReason::EndTurn);
    let turn_id = turn.id.clone();
    let mut f = Fixture::new(80, 24, turn);
    // Completed tool-dominant turns default-fold. Explicitly expand through
    // the production paired view/cache effect before asking Stage to focus.
    let tab_id = f.tabs.active_tab_id();
    chat_pane::set_turn_collapsed(
        &mut f.tabs.active_tab_mut().view_state,
        f.h.state.tab_render_states.get_mut(&tab_id).unwrap(),
        &turn_id,
        false,
    );
    f.paint();
    (f, [first, second, third])
}

fn tab(f: &mut Fixture) -> CycleOutcome {
    assert_eq!(f.press(Key::Tab), InputAction::CycleInvocationInFocusedTurn);
    let active = f.tabs.active_tab_mut();
    let outcome = cycle_item(
        &mut f.h.state,
        &mut active.view_state,
        &active.conversation.turns,
        active.reducer.open_turn.as_ref(),
    );
    if let CycleOutcome::Cycled { item_id } = &outcome {
        // Exactly the host's select_item_explicitly + BlockJump snapshots.
        // Only the production helper may seat the canonical owner or scroll.
        f.h.state.select_item_explicitly(item_id.clone());
        f.h.state.set_scroll_offset(active.view_state.scroll_offset);
        f.h.state
            .set_auto_scroll(matches!(active.view_state.mode, AnchorMode::Following));
        f.h.state.needs_redraw = true;
    }
    outcome
}

fn cycled(f: &mut Fixture, expected: &str) {
    match tab(f) {
        CycleOutcome::Cycled { item_id } => assert_eq!(item_id, expected),
        CycleOutcome::Fallthrough => panic!("Tab failed to reach {expected}"),
    }
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(expected));
    assert_eq!(f.h.state.selected_item_id.as_deref(), Some(expected));
    f.paint();
    assert_eq!(
        f.h.state.focused_item_id.as_deref(),
        Some(expected),
        "the next real render must not reseat an offscreen selection"
    );
}

fn inspect_and_back(f: &mut Fixture, expected: &str, title: &str, output: &str) {
    let view = f.view();
    let cache = f.cache();
    f.open();
    assert_eq!(f.h.state.modal_stack.top().unwrap().title(), title);
    assert!(f.h.screen_text().contains(output));
    assert_eq!(f.press(Key::Esc), InputAction::Consumed);
    f.paint();
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(expected));
    assert_eq!(f.view(), view);
    assert_eq!(f.cache(), cache);
}

fn viewport_top(f: &Fixture) -> usize {
    f.h.state
        .total_content_height
        .saturating_sub(usize::from(f.h.state.viewport_height))
        .saturating_sub(f.tabs.active_tab().view_state.scroll_offset)
}

fn single_turn_header(f: &Fixture, id: &str) -> usize {
    assert_eq!(f.conversation().turns.len(), 1);
    let view = &f.tabs.active_tab().view_state;
    let key = HeightKey {
        turn_id: f.conversation().turns[0].id.clone(),
        expansion: true,
        summary_tier: view.summary_tier,
        terminal_width: f.layout().chat_pane.width,
    };
    let layout = &f.h.state.tab_render_states[&f.tabs.active_tab_id()]
        .height_cache
        .entries[&key];
    layout
        .tool_block_offsets
        .iter()
        .find(|(_, tool)| tool == id)
        .unwrap()
        .0
}

fn unchanged_state(f: &Fixture) -> String {
    let s = &f.h.state;
    format!(
        "{:?}",
        (
            (f.view(), f.cache(), f.tabs.active_tab_id(), &s.focus),
            (
                &s.focused_item_id,
                &s.selected_item_id,
                s.scroll_offset(),
                s.auto_scroll()
            ),
            (
                s.needs_redraw,
                s.pending_anchor,
                s.total_content_height,
                s.viewport_height
            ),
            (
                &s.block_boundaries,
                &s.message_boundaries,
                &s.user_message_boundaries
            ),
            (&s.input_buffer, s.cursor_position, s.input_scroll_offset),
        )
    )
}

fn fallthrough_unchanged(f: &mut Fixture) {
    // Snapshot the actual owning view, not a detached default ViewState.
    let before = unchanged_state(f);
    let transcript = serde_json::to_vec(f.conversation()).unwrap();
    let stage = pixels(f.buffer(), f.layout().chat_pane);
    assert!(matches!(tab(f), CycleOutcome::Fallthrough));
    assert_eq!(
        unchanged_state(f),
        before,
        "Fallthrough must leave selection, actual ViewState and host snapshots alone"
    );
    assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), transcript);
    assert_eq!(pixels(f.buffer(), f.layout().chat_pane), stage);
}

#[test]
fn single_assistant_turn_tab_reaches_every_tool() {
    let (mut f, ids) = three(false);
    f.focus_call(&ids[0]);
    assert!(f.tabs.active_tab().view_state.focused_turn.is_none());
    let cache = f.cache();
    assert!(
        !cache.0.is_empty(),
        "real Stage layout/cache positive control"
    );
    let height = f.h.state.total_content_height;
    let transcript = serde_json::to_vec(f.conversation()).unwrap();
    for (index, title, output) in [
        (1, "Tool · Edit #2", "SECOND-OUTPUT"),
        (2, "Tool · Write #3", "THIRD-OUTPUT"),
        (0, "Tool · Bash #1", "FIRST-OUTPUT"),
    ] {
        cycled(&mut f, &ids[index]);
        assert_eq!(
            f.tabs.active_tab().view_state.focused_turn.as_ref(),
            Some(&f.conversation().turns[0].id),
            "A11 seats the exact qualifying owner"
        );
        inspect_and_back(&mut f, &ids[index], title, output);
        assert_eq!(f.h.state.total_content_height, height);
        assert_eq!(f.cache(), cache);
        assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), transcript);
    }
}

#[test]
fn offscreen_next_header_and_wrap_use_bottom_offsets_and_survive_real_render() {
    // Exercise both the new owner fallback and the pre-existing explicit seat.
    for explicit_turn in [false, true] {
        let (mut f, ids) = three(true);
        let turn_id = f.conversation().turns[0].id.clone();
        if explicit_turn {
            f.tabs.active_tab_mut().view_state.focused_turn = Some(turn_id.clone());
        }
        f.focus_call(&ids[0]);
        let second_start = single_turn_header(&f, &ids[1]);
        let before_top = viewport_top(&f);
        assert!(
            second_start >= before_top + usize::from(f.h.state.viewport_height),
            "the 60 actual error rows must put header 2 below the viewport"
        );
        assert!(!f.h.screen_text().contains("k10-second"));
        assert!(f.h.screen_text().contains("k10-first"));
        let cache = f.cache();
        let height = f.h.state.total_content_height;
        let rect = f.layout().chat_pane;
        let max = height.saturating_sub(usize::from(f.h.state.viewport_height));
        cycled(&mut f, &ids[1]);
        assert_eq!(
            f.tabs.active_tab().view_state.scroll_offset,
            max.saturating_sub(second_start),
            "J/K offset-from-bottom conversion"
        );
        assert_eq!(
            f.h.state.scroll_offset(),
            f.tabs.active_tab().view_state.scroll_offset
        );
        assert!(!f.h.state.auto_scroll());
        assert!(matches!(
            f.tabs.active_tab().view_state.mode,
            AnchorMode::Reading
        ));
        assert_eq!(viewport_top(&f), second_start);
        assert!(
            row(f.buffer(), rect, rect.y).contains("k10-second"),
            "the cached start must correspond to an actually painted header"
        );
        inspect_and_back(&mut f, &ids[1], "Tool · Edit #2", "SECOND-OUTPUT");
        cycled(&mut f, &ids[2]);
        let first_start = single_turn_header(&f, &ids[0]);
        assert!(
            first_start < viewport_top(&f),
            "wrap target is genuinely above the viewport"
        );
        assert!(!f.h.screen_text().contains("k10-first"));
        cycled(&mut f, &ids[0]);
        assert_eq!(
            f.tabs.active_tab().view_state.scroll_offset,
            max.saturating_sub(first_start)
        );
        assert_eq!(viewport_top(&f), first_start);
        assert!(row(f.buffer(), rect, rect.y).contains("k10-first"));
        assert!(matches!(
            f.tabs.active_tab().view_state.mode,
            AnchorMode::Reading
        ));
        assert!(!f.h.state.auto_scroll());
        assert_eq!(f.h.state.scroll_offset(), max.saturating_sub(first_start));
        assert_eq!(f.tabs.active_tab().view_state.focused_turn, Some(turn_id));
        assert_eq!(f.h.state.total_content_height, height);
        assert_eq!(
            f.cache(),
            cache,
            "Tab changes focus/scroll, never Stage layout keys or rows"
        );
        assert_eq!(f.layout().chat_pane, rect);
    }
}

#[test]
fn tab_to_last_viewport_header_restores_following() {
    let mut turn = Turn::new("bottom-model".into(), 0);
    prose(&mut turn, &rows(40));
    let first = call(
        &mut turn,
        "Bash",
        json!({"command":"bottom-first"}),
        S::Success,
        None,
    );
    let last = call(
        &mut turn,
        "Edit",
        json!({"file_path":"bottom-last"}),
        S::Success,
        None,
    );
    prose(&mut turn, "After the last header.");
    let mut f = Fixture::new(80, 24, turn);
    f.focus_call(&first);
    let header = single_turn_header(&f, &last);
    let viewport = usize::from(f.h.state.viewport_height);
    let max = f.h.state.total_content_height.saturating_sub(viewport);
    assert!(header > max, "last header lies within the final viewport");
    let top = header.saturating_sub(viewport);
    assert!(top > 0 && top < max);
    let offset = max - top;
    f.tabs.active_tab_mut().view_state.scroll_offset = offset;
    f.tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
    f.h.state.set_scroll_offset(offset);
    f.h.state.set_auto_scroll(false);
    f.paint();
    assert_eq!(
        viewport_top(&f) + viewport,
        header,
        "target is just below viewport"
    );
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(first.as_str()));
    assert!(matches!(tab(&mut f), CycleOutcome::Cycled { item_id } if item_id == last));
    assert_eq!(f.tabs.active_tab().view_state.scroll_offset, 0);
    assert!(matches!(
        f.tabs.active_tab().view_state.mode,
        AnchorMode::Following
    ));
    assert!(
        f.h.state.auto_scroll(),
        "host auto_snapshot follows the owning view"
    );
    f.paint();
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(last.as_str()));
}

#[test]
fn tab_cycles_and_inspects_canonical_open_turn() {
    let (mut f, _) = Fixture::single(80, 24, "Bash", json!({}), S::Success, None);
    let mut open = Turn::new("streaming-model".into(), 0);
    let first = call(
        &mut open,
        "Bash",
        json!({"command":"open-first"}),
        S::Running,
        None,
    );
    let second = call(
        &mut open,
        "Edit",
        json!({"file_path":"open-second"}),
        S::Running,
        None,
    );
    let open_id = open.id.clone();
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.tabs.active_tab_mut().reducer.open_turn = Some(open);
    {
        let active = f.tabs.active_tab_mut();
        rustain::domain::services::reducer::update_streaming_mirror(
            &active.reducer,
            &mut active.streaming,
        );
    }
    assert!(f.conversation().turns.is_empty(), "no committed duplicate");
    f.focus_call(&first);
    cycled(&mut f, &second);
    assert_eq!(
        f.tabs.active_tab().view_state.focused_turn.as_ref(),
        Some(&open_id)
    );
    f.open();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().title(),
        "Tool · Edit #2"
    );
    assert_eq!(f.tool().target.tool_call_id, second);
    assert_eq!(f.press(Key::Esc), InputAction::Consumed);
    f.paint();
    cycled(&mut f, &first);
    f.open();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().title(),
        "Tool · Bash #1"
    );
}

#[test]
fn focused_expanded_turn_keeps_original_cycle_and_visible_scroll_position() {
    let (mut f, ids) = three(false);
    let turn_id = f.conversation().turns[0].id.clone();
    f.tabs.active_tab_mut().view_state.focused_turn = Some(turn_id.clone());
    f.focus_call(&ids[0]);
    cycled(&mut f, &ids[1]);
    let view = f.view();
    let cache = f.cache();
    cycled(&mut f, &ids[2]);
    cycled(&mut f, &ids[0]);
    assert_eq!(
        f.view(),
        view,
        "visible targets do not need a scroll or mode change"
    );
    assert_eq!(f.cache(), cache);
    assert_eq!(f.tabs.active_tab().view_state.focused_turn, Some(turn_id));
    inspect_and_back(&mut f, &ids[0], "Tool · Bash #1", "FIRST-OUTPUT");
}

#[test]
fn owner_resolution_uses_exact_canonical_id_and_never_crosses_turns() {
    let mut prefix = Turn::new("prefix-model".into(), 0);
    prefix.id = TurnId("owner".into());
    let other = call(
        &mut prefix,
        "Bash",
        json!({"command":"prefix-first"}),
        S::Success,
        Some(("OTHER-TURN", D::NotAWrite)),
    );
    call(
        &mut prefix,
        "Write",
        json!({"file_path":"prefix-last"}),
        S::Success,
        None,
    );
    prose(&mut prefix, &rows(15));
    let mut owner = Turn::new("owner-model".into(), 0);
    owner.id = TurnId("owner_suffix".into());
    let first = call(
        &mut owner,
        "Edit",
        json!({"file_path":"exact-first"}),
        S::Success,
        Some(("EXACT-FIRST", D::NotAWrite)),
    );
    let second = call(
        &mut owner,
        "Write",
        json!({"file_path":"exact-second"}),
        S::Success,
        Some(("EXACT-SECOND", D::NewFile)),
    );
    let mut f = Fixture::new(80, 24, prefix);
    f.tabs.active_tab_mut().conversation.turns.push(owner);
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.focus_call(&first);
    assert!(f.tabs.active_tab().view_state.focused_turn.is_none());
    let cache = f.cache();
    cycled(&mut f, &second);
    inspect_and_back(&mut f, &second, "Tool · Write #2", "EXACT-SECOND");
    cycled(&mut f, &first);
    assert_ne!(f.h.state.selected_item_id.as_deref(), Some(other.as_str()));
    assert_eq!(
        f.tabs.active_tab().view_state.focused_turn.as_ref(),
        Some(&f.conversation().turns[1].id),
        "never a prefix-colliding owner"
    );
    assert_eq!(f.cache(), cache);
    // An existing explicit turn seat still takes precedence over the new
    // render-owner fallback. A render-focused id outside that turn starts at
    // its first invocation, preserving the original gate/selection rule.
    let explicit = f.conversation().turns[0].id.clone();
    f.tabs.active_tab_mut().view_state.focused_turn = Some(explicit.clone());
    f.paint();
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(first.as_str()));
    cycled(&mut f, &other);
    assert_eq!(f.tabs.active_tab().view_state.focused_turn, Some(explicit));
    inspect_and_back(&mut f, &other, "Tool · Bash #1", "OTHER-TURN");
}

#[test]
fn zero_and_single_invocation_preserve_sidebar_and_tab_fallthrough() {
    for count in [0, 1] {
        for sidebar in [false, true] {
            let mut turn = Turn::new("fallback-model".into(), 0);
            if count == 1 {
                call(
                    &mut turn,
                    "Bash",
                    json!({"command":"only-call"}),
                    S::Success,
                    None,
                );
            } else {
                prose(&mut turn, "A real assistant turn without tool invocations.");
            }
            let mut f = Fixture::new(80, 24, turn);
            // Both fallback destinations are available; cycle must not consume
            // either by silently selecting a tool. Dispatch itself stays host-owned.
            f.tabs.create_tab();
            f.tabs.switch_to_prev();
            f.load();
            f.h.state.monitor = sidebar;
            f.h.state.focus = FocusState::Chat;
            f.paint();
            assert_eq!(f.tabs.tab_count(), 2);
            assert_eq!(f.h.state.focused_item_id.is_some(), count == 1);
            for focused in [false, true] {
                f.tabs.active_tab_mut().view_state.focused_turn =
                    focused.then(|| f.conversation().turns[0].id.clone());
                f.paint();
                fallthrough_unchanged(&mut f);
            }
            // A real multi-call focused turn, with the same sidebar setting,
            // positively proves the decoder/effect can select and paint a tool.
            let (mut control, ids) = three(false);
            control.h.state.monitor = sidebar;
            control.tabs.active_tab_mut().view_state.focused_turn =
                Some(control.conversation().turns[0].id.clone());
            control.focus_call(&ids[0]);
            cycled(&mut control, &ids[1]);
            inspect_and_back(&mut control, &ids[1], "Tool · Edit #2", "SECOND-OUTPUT");
        }
    }
}

#[test]
fn collapsed_turn_falls_through_but_the_same_expanded_turn_cycles() {
    let (mut f, ids) = three(false);
    let id = f.conversation().turns[0].id.clone();
    f.tabs.active_tab_mut().view_state.focused_turn = Some(id.clone());
    f.focus_call(&ids[0]);
    cycled(&mut f, &ids[1]); // Observable positive before hiding the same calls.
    // The content-aware default fold is also a real collapsed Stage, even
    // though ViewState::is_collapsed alone reports its default as expanded.
    f.tabs.active_tab_mut().view_state.collapsed.remove(&id);
    let tab_id = f.tabs.active_tab_id();
    f.h.state
        .tab_render_states
        .get_mut(&tab_id)
        .unwrap()
        .height_cache
        .invalidate_turn(&id);
    f.paint();
    assert!(f.h.state.focused_item_id.is_none());
    assert!(!f.h.screen_text().contains("k10-second"));
    fallthrough_unchanged(&mut f);
    {
        let tab_id = f.tabs.active_tab_id();
        chat_pane::set_turn_collapsed(
            &mut f.tabs.active_tab_mut().view_state,
            f.h.state.tab_render_states.get_mut(&tab_id).unwrap(),
            &id,
            true,
        );
    }
    f.paint();
    assert!(
        f.tabs
            .active_tab()
            .view_state
            .is_collapsed(&f.conversation().turns[0])
    );
    assert!(
        f.h.state.focused_item_id.is_none(),
        "collapsed Stage has no tool headers"
    );
    fallthrough_unchanged(&mut f);
    f.tabs.active_tab_mut().view_state.focused_turn = None;
    f.paint();
    fallthrough_unchanged(&mut f);
    {
        let tab_id = f.tabs.active_tab_id();
        chat_pane::set_turn_collapsed(
            &mut f.tabs.active_tab_mut().view_state,
            f.h.state.tab_render_states.get_mut(&tab_id).unwrap(),
            &id,
            false,
        );
    }
    f.focus_call(&ids[0]);
    cycled(&mut f, &ids[1]);
    assert_eq!(f.tabs.active_tab().view_state.focused_turn, Some(id));
}

#[test]
fn no_visible_focus_does_not_guess_a_multi_tool_turn() {
    let (mut f, ids) = three(true);
    f.focus_call(&ids[0]);
    let header = single_turn_header(&f, &ids[0]);
    let max =
        f.h.state
            .total_content_height
            .saturating_sub(usize::from(f.h.state.viewport_height));
    // Inside the first error body, no header starts in the viewport.
    let offset = max.saturating_sub(header + 3);
    f.tabs.active_tab_mut().view_state.scroll_offset = offset;
    f.h.state.set_scroll_offset(offset);
    f.paint();
    assert!(f.h.state.focused_item_id.is_none());
    assert!(
        f.h.screen_text().contains("row 003"),
        "real error-body viewport control"
    );
    fallthrough_unchanged(&mut f);
    f.focus_call(&ids[0]);
    cycled(&mut f, &ids[1]);
    inspect_and_back(&mut f, &ids[1], "Tool · Edit #2", "SECOND-OUTPUT");
}

#[test]
fn removed_render_focused_owner_does_not_select_from_another_turn() {
    let (mut f, ids) = three(false);
    let owner = f.conversation().turns[0].id.clone();
    let mut decoy = Turn::new("decoy-model".into(), 0);
    prose(&mut decoy, &rows(18));
    call(
        &mut decoy,
        "Bash",
        json!({"command":"decoy-first"}),
        S::Success,
        None,
    );
    call(
        &mut decoy,
        "Write",
        json!({"file_path":"decoy-last"}),
        S::Success,
        None,
    );
    f.tabs.active_tab_mut().conversation.turns.push(decoy);
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.focus_call(&ids[0]);
    cycled(&mut f, &ids[1]);
    inspect_and_back(&mut f, &ids[1], "Tool · Edit #2", "SECOND-OUTPUT");
    // Model the render-to-key window: history removes the owning turn after
    // the last real Stage paint. Keep that authentic render-derived id; painting
    // again here would replace the unresolved-id case with merely absent focus.
    f.tabs
        .active_tab_mut()
        .conversation
        .turns
        .retain(|turn| turn.id != owner);
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    assert_eq!(f.h.state.focused_item_id.as_deref(), Some(ids[1].as_str()));
    assert_eq!(
        f.tabs.active_tab().view_state.focused_turn.as_ref(),
        Some(&owner)
    );
    assert_eq!(
        f.conversation().turns.len(),
        1,
        "another multi-tool turn remains"
    );
    fallthrough_unchanged(&mut f);
    // Also exercise the no-seated-turn fallback with the same authentic stale id.
    f.tabs.active_tab_mut().view_state.set_focused_turn(None);
    assert!(f.tabs.active_tab().view_state.focused_turn.is_none());
    fallthrough_unchanged(&mut f);
}
