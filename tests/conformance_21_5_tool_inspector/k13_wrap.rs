//! K13 / M12: wrap changes real body cells, never the owning Stage.
use super::support::*;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use rustain::adapters::tui::{
    app::InputAction,
    decision_pane::{self, Arrival, DecisionDraft, DecisionKind, Raised},
    keymap::Key,
    markdown::metrics,
    presentation::{self, Surface},
    tool_inspector::{self, ToolTab},
};
use rustain::domain::models::{InvocationStatus as S, Turn, WriteDiffState as D};
use rustain::domain::models::{RequestId, ToolRisk, tool_call::ApprovalSource};
use rustain::domain::services::approval_runtime::ApprovalRuntimeEvent;
use rustain::infrastructure::runtime::approval_bridge;
use serde_json::json;
use std::time::Duration;

fn output(text: &str, width: u16, height: u16) -> Fixture {
    let (mut f, id) = Fixture::single(
        width,
        height,
        "Bash",
        json!({"command":"wrap witness"}),
        S::Success,
        Some((text, D::NotAWrite)),
    );
    f.focus_call(&id);
    f
}

fn toggle(f: &mut Fixture) {
    assert_eq!(f.press(Key::Char('w')), InputAction::Consumed);
    f.paint();
}

fn last_cell(f: &Fixture, offset: u16) -> &str {
    let b = f.body_rect();
    f.buffer()[(b.right() - 1, b.y + offset)].symbol()
}

fn logical_rows(count: usize) -> String {
    (0..count)
        .map(|n| format!("line{n:02}:{}", "x".repeat(193)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn wrap_off_clips_with_an_arrow_and_keeps_the_stage() {
    let text = format!("{}\nFITTING-ROW", "x".repeat(200));
    let mut f = output(&text, 80, 24);
    f.h.state.input_buffer = "unchanged draft".into();
    f.h.state.cursor_position = 4;
    f.paint();
    let layout = f.layout();
    let stage = pixels(f.buffer(), layout.chat_pane);
    let mut composer = pixels(f.buffer(), layout.input_area);
    for cell in &mut composer {
        cell.modifier.insert(Modifier::DIM);
    }
    let cache = f.cache();
    let view = f.view();
    let height = f.h.state.total_content_height;
    let transcript = serde_json::to_vec(f.conversation()).unwrap();
    let draft = (
        f.h.state.input_buffer.clone(),
        f.h.state.cursor_position,
        f.h.state.input_scroll_offset,
    );
    assert!(!cache.0.is_empty(), "real Stage cache control");
    f.open();
    let width = usize::from(f.body_rect().width);
    let wrapped = 200usize.div_ceil(width);
    assert_eq!(wrapped, 3);
    assert_eq!(
        f.tool().rows().len(),
        wrapped + 2,
        "fitting row and Literal separator"
    );
    assert_eq!(f.body_row(0), "x".repeat(width));
    assert!(f.body_row(wrapped as u16).starts_with("FITTING-ROW"));
    let key_on = f.tool().bodies[0].as_ref().unwrap().0.clone();
    let on_pixels = pixels(f.buffer(), f.body_rect());
    toggle(&mut f);
    assert_eq!(
        f.tool().rows().len(),
        3,
        "one long logical row, fitting control, separator"
    );
    assert_eq!(f.body_row(0), format!("{}→", "x".repeat(width - 1)));
    assert_eq!(
        last_cell(&f, 0),
        "→",
        "M12: clipped rows need the last-cell marker"
    );
    assert!(f.body_row(1).starts_with("FITTING-ROW"));
    assert_eq!(
        last_cell(&f, 1),
        " ",
        "fitting rows must not acquire a marker"
    );
    let key_off = f.tool().bodies[0].as_ref().unwrap().0.clone();
    assert_ne!(key_off.content_hash, key_on.content_hash);
    assert_eq!(usize::from(key_on.terminal_width), width);
    assert_eq!(
        key_off.terminal_width, 0,
        "unwrapped rendering has a width-independent key"
    );
    assert_eq!(key_off.msg_id, key_on.msg_id);
    assert_eq!(f.tool().heights.message_entries.get(&key_off), Some(&3));
    assert_eq!(f.cache(), cache, "wrap-off cannot rekey the Stage");
    assert_eq!(f.view(), view);
    assert_eq!(f.h.state.total_content_height, height);
    assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), transcript);
    assert_eq!(
        (
            f.h.state.input_buffer.clone(),
            f.h.state.cursor_position,
            f.h.state.input_scroll_offset
        ),
        draft
    );
    assert_eq!(pixels(f.buffer(), f.layout().input_area), composer);
    toggle(&mut f);
    assert_eq!(f.tool().bodies[0].as_ref().unwrap().0, key_on);
    assert_eq!(pixels(f.buffer(), f.body_rect()), on_pixels);
    assert_eq!(f.cache(), cache);
    assert_eq!(f.view(), view);
    assert_eq!(f.h.state.total_content_height, height);
    assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), transcript);
    assert_eq!(
        (
            f.h.state.input_buffer.clone(),
            f.h.state.cursor_position,
            f.h.state.input_scroll_offset
        ),
        draft
    );
    assert_eq!(pixels(f.buffer(), f.layout().input_area), composer);
    f.press(Key::Esc);
    f.paint();
    assert_eq!(pixels(f.buffer(), layout.chat_pane), stage);
}

#[test]
fn every_real_tab_wraps_by_default_and_reuses_its_warm_wrap_key() {
    let long = "L".repeat(200);
    let content = format!("{long}\nFIT");
    let (mut f, id) = Fixture::single(
        80,
        30,
        "Write",
        json!({"file_path":"wrap.txt", "content":content}),
        S::Success,
        Some((&content, D::NewFile)),
    );
    f.focus_call(&id);
    f.open();
    let stage = f.cache();
    for tab in [ToolTab::Output, ToolTab::Input, ToolTab::Raw, ToolTab::Diff] {
        while f.tool().tab != tab {
            f.press(Key::Right);
            f.paint();
        }
        f.press(Key::Char('g'));
        f.paint();
        let on_rows = f.tool().rows().len();
        let width = usize::from(f.body_rect().width);
        assert!(f.tool().rows().iter().all(|line| line.width() <= width));
        let on_key = f.tool().bodies[tab.index()].as_ref().unwrap().0.clone();
        metrics::reset();
        assert_eq!(f.press(Key::Char('w')), InputAction::Consumed);
        for _ in 0..5 {
            f.compose();
        }
        assert!(
            metrics::renders() <= 1,
            "at most one shared render per {tab:?} key"
        );
        if tab != ToolTab::Diff {
            assert_eq!(
                metrics::renders(),
                1,
                "cold Literal instrumentation control"
            );
        }
        let off_rows = f.tool().rows().len();
        assert!(
            off_rows < on_rows,
            "{tab:?} must really wrap its long source by default"
        );
        assert!(f.tool().rows().iter().any(|line| line.width() > width));
        let off_key = f.tool().bodies[tab.index()].as_ref().unwrap().0.clone();
        assert_ne!(on_key.content_hash, off_key.content_hash);
        if tab == ToolTab::Diff {
            assert!(f.body_row(0).starts_with("+ L"), "actual long Diff content");
            assert!(f.body_row(1).starts_with("+ FIT"));
            assert_eq!(last_cell(&f, 1), " ", "fitting Diff row must not be marked");
        }
        let mut saw_arrow = false;
        for _ in 0..off_rows {
            f.paint();
            saw_arrow |= (0..f.body_rect().height).any(|y| last_cell(&f, y) == "→");
            f.press(Key::Down);
        }
        assert!(
            saw_arrow,
            "{tab:?} needs an observable long-row clipping control"
        );
        metrics::reset();
        for _ in 0..5 {
            f.compose();
        }
        assert_eq!(
            metrics::renders(),
            0,
            "warm {tab:?} repaint must reuse its rows"
        );
        // Visit a neighbouring source and return: its paint cannot evict this tab.
        let (away, back) = if tab == ToolTab::Diff {
            (Key::Left, Key::Right)
        } else {
            (Key::Right, Key::Left)
        };
        f.press(away);
        f.compose();
        metrics::reset();
        f.press(back);
        f.compose();
        assert_eq!(f.tool().tab, tab);
        assert_eq!(metrics::renders(), 0, "per-tab memo for {tab:?}");
        assert_eq!(f.tool().bodies[tab.index()].as_ref().unwrap().0, off_key);
        f.press(Key::Char('g'));
        toggle(&mut f);
        assert_eq!(f.tool().rows().len(), on_rows);
        assert_eq!(f.tool().bodies[tab.index()].as_ref().unwrap().0, on_key);
        assert_eq!(f.cache(), stage);
    }
}

#[test]
fn clipping_preserves_unicode_graphemes_tab_stops_and_crlf_lines() {
    // At 80 columns the actual standard frame has 78 inner cells.
    let width = 78usize;
    let edge = "a".repeat(width - 2);
    let exact = "z".repeat(width);
    let text = format!(
        "A\t界e\u{301}👩\u{200d}💻{}\r\n界e\u{301}👩\u{200d}💻\tZ\r\n{edge}界tail\r\n{edge}e\u{301}tail\r\n{edge}👩\u{200d}💻tail\r\n{exact}",
        "X".repeat(200)
    );
    let mut f = output(&text, 80, 30);
    f.open();
    assert_eq!(usize::from(f.body_rect().width), width);
    assert!(
        f.tool().rows().len() > 7,
        "wrapped long Unicode source control"
    );
    toggle(&mut f);
    assert_eq!(
        f.tool().rows().len(),
        7,
        "six CRLF lines plus one Literal separator"
    );
    let b = f.body_rect();
    let cell = |x: u16, y: u16| f.buffer()[(b.x + x, b.y + y)].symbol();
    assert_eq!(cell(0, 0), "A");
    for x in 1..8 {
        assert_eq!(cell(x, 0), " ");
    }
    assert_eq!(cell(8, 0), "界");
    assert_eq!(cell(10, 0), "e\u{301}");
    assert_eq!(cell(11, 0), "👩\u{200d}💻");
    assert_eq!(last_cell(&f, 0), "→");
    assert_eq!(cell(0, 1), "界");
    assert_eq!(cell(2, 1), "e\u{301}");
    assert_eq!(cell(3, 1), "👩\u{200d}💻");
    for x in 5..8 {
        assert_eq!(cell(x, 1), " ");
    }
    assert_eq!(cell(8, 1), "Z");
    assert_eq!(last_cell(&f, 1), " ");
    for y in [2, 4] {
        assert_eq!(
            cell((width - 2) as u16, y),
            " ",
            "never paint half a wide grapheme"
        );
        assert_eq!(last_cell(&f, y), "→");
    }
    assert_eq!(cell((width - 2) as u16, 3), "e\u{301}");
    assert_eq!(last_cell(&f, 3), "→");
    assert_eq!(
        f.body_row(5),
        exact,
        "an exactly fitting row has no clip marker"
    );
    let before = pixels(f.buffer(), b);
    for key in [Key::Char('h'), Key::Char('l')] {
        f.press(key);
        f.paint();
    }
    assert_eq!(
        pixels(f.buffer(), b),
        before,
        "21-13, not wrap, owns horizontal scrolling"
    );
    toggle(&mut f);
    assert_eq!(f.body_row(0).chars().next(), Some('A'));
}

#[test]
fn toggle_anchors_the_logical_line_both_ways_and_navigation_clamps() {
    let mut f = output(&logical_rows(40), 80, 24);
    f.open();
    let wraps = 200usize.div_ceil(usize::from(f.body_rect().width));
    f.press(Key::PageDown);
    f.paint();
    assert_eq!(
        f.tool().viewers[0].top,
        f.tool().viewport.saturating_sub(1).max(1)
    );
    f.press(Key::Char('g'));
    for _ in 0..(4 * wraps + 1) {
        f.press(Key::Down);
    }
    f.paint();
    assert_eq!(f.tool().viewers[0].top, 4 * wraps + 1);
    assert!(
        f.body_row(0).starts_with('x'),
        "start in a real soft-wrap continuation"
    );
    toggle(&mut f);
    assert_eq!(f.tool().viewers[0].top, 4);
    assert!(f.body_row(0).starts_with("line04:"));
    toggle(&mut f);
    assert_eq!(f.tool().viewers[0].top, 4 * wraps);
    assert!(f.body_row(0).starts_with("line04:"));
    toggle(&mut f);
    for _ in 0..50 {
        f.press(Key::PageDown);
    }
    f.paint();
    let maximum = f.tool().rows().len().saturating_sub(f.tool().viewport);
    assert!(maximum > 0);
    assert_eq!(f.tool().viewers[0].top, maximum);
    f.press(Key::Down);
    f.paint();
    assert_eq!(f.tool().viewers[0].top, maximum);
    f.press(Key::Up);
    f.paint();
    assert_eq!(f.tool().viewers[0].top, maximum - 1);
    f.press(Key::PageUp);
    f.paint();
    assert_eq!(
        f.tool().viewers[0].top,
        (maximum - 1).saturating_sub(f.tool().viewport.saturating_sub(1).max(1))
    );
    toggle(&mut f);
    assert!(f.tool().viewers[0].top <= f.tool().rows().len().saturating_sub(f.tool().viewport));
    f.press(Key::Char('g'));
    f.paint();
    assert_eq!(f.tool().viewers[0].top, 0);
    assert!(f.body_row(0).starts_with("line00:"));
}

#[test]
fn same_turn_step_keeps_wrap_off_but_clears_query_and_all_viewers() {
    let mut turn = Turn::new("wrap-model".into(), 0);
    let content = logical_rows(40);
    let first = call(
        &mut turn,
        "Bash",
        json!({"items":(0..40).collect::<Vec<_>>()}),
        S::Success,
        Some((&content, D::NotAWrite)),
    );
    prose(&mut turn, &rows(20));
    let second = call(
        &mut turn,
        "Bash",
        json!({"items":(0..40).collect::<Vec<_>>()}),
        S::Success,
        Some((&content, D::NotAWrite)),
    );
    let mut f = Fixture::new(80, 24, turn);
    super::k11_search::theme(&mut f);
    f.focus_call(&first);
    f.open();
    let frame = f.h.state.modal_stack.top().unwrap().id;
    let stage_focus = f.h.state.focused_tool_id.clone();
    toggle(&mut f);
    assert_eq!(last_cell(&f, 0), "→");
    for _ in 0..2 {
        f.press(Key::Right);
        f.press(Key::PageDown);
        f.paint();
        assert!(f.tool().viewers[f.tool().tab.index()].top > 0);
    }
    f.press(Key::Left);
    f.press(Key::Left);
    f.paint();
    f.press(Key::Char('/'));
    for c in "line03".chars() {
        f.press(Key::Char(c));
    }
    f.press(Key::Enter);
    f.paint();
    assert!(
        f.h.screen_text().contains("Search 1/1"),
        "committed query control"
    );
    assert_eq!(f.tool().viewers[0].top, 3);
    assert_eq!(f.tool().search.query(), "line03");
    assert_eq!(f.tool().search.matches.len(), 1);
    assert!(
        super::k11_search::marked_cells(&f) > 0,
        "real painted search control"
    );
    let InputAction::StepTool { forward } = f.press(Key::Char(']')) else {
        panic!("real decoder must request same-turn stepping");
    };
    let tab = f.tabs.active_tab();
    tool_inspector::step(
        &mut f.h.state,
        &tab.conversation,
        tab.reducer.open_turn.as_ref(),
        forward,
    );
    f.paint();
    assert_eq!(f.h.state.modal_stack.top().unwrap().id, frame);
    assert_eq!(f.tool().target.tool_call_id, second);
    assert!(f.tool().viewers.iter().all(|v| v.top == 0 && !v.follow));
    assert!(f.body_row(0).starts_with("line00:"));
    assert_eq!(
        last_cell(&f, 0),
        "→",
        "step preserves the actual clipping mode"
    );
    assert!(
        !super::k11_search::status(&f).contains("Search"),
        "the committed query status disappears, not the valid registry Search hint"
    );
    assert!(f.tool().search.query().is_empty());
    assert!(f.tool().search.matches.is_empty());
    assert!(!f.tool().search.editor.editing);
    assert!(f.tool().search.editor.query.as_str().is_empty());
    let before = pixels(f.buffer(), f.body_rect());
    let viewers = f.tool().viewers;
    f.press(Key::Char('n'));
    f.paint();
    assert_eq!(
        pixels(f.buffer(), f.body_rect()),
        before,
        "cleared query cannot jump"
    );
    assert_eq!(
        f.tool().viewers,
        viewers,
        "cleared query cannot move any viewer"
    );
    assert_eq!(f.h.state.focused_tool_id, stage_focus);
}

#[test]
fn exact_target_reopen_and_automatic_preemption_restore_the_off_mode() {
    let mut f = output(&logical_rows(40), 80, 24);
    f.open();
    toggle(&mut f);
    f.press(Key::PageDown);
    f.paint();
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    let top = f.tool().viewers[0].top;
    assert!(top > 0);
    assert_eq!(last_cell(&f, 0), "→");
    let body = pixels(f.buffer(), f.body_rect());
    let key = f.tool().bodies[0].as_ref().unwrap().0.clone();
    assert!(f.inspect(), "exact-target production reopen control");
    f.paint();
    assert_eq!(f.h.state.modal_stack.len(), 1);
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert_eq!(pixels(f.buffer(), f.body_rect()), body);
    assert!(f.h.state.decisions.is_empty());
    f.clock.advance(Duration::from_secs(2));
    assert_eq!(
        decision_pane::raise(
            &mut f.h.state,
            DecisionDraft::new(DecisionKind::Budget, "wrap preemption control"),
            Arrival::Automatic
        ),
        Raised::Focused
    );
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::DecisionPane
    );
    assert!(f.h.screen_text().contains("wrap preemption control"));
    f.clock.advance(decision_pane::INERT_AFTER_PAINT);
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert_eq!(f.tool().viewers[0].top, top);
    assert_eq!(f.tool().bodies[0].as_ref().unwrap().0, key);
    assert_eq!(pixels(f.buffer(), f.body_rect()), body);
    toggle(&mut f);
    assert_ne!(
        last_cell(&f, 0),
        "→",
        "restored mode can toggle back to real wrapping"
    );
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn narrow_full_composer_keeps_primary_body_back_rail_and_ready_uncovered() {
    let mut f = output(&format!("{}\nFIT", "x".repeat(200)), 60, 16);
    f.h.state.input_buffer = ["draft"; 8].join("\n");
    f.h.state.cursor_position = 5;
    f.h.state.session_count = u32::MAX;
    f.paint();
    let layout = f.layout();
    assert_eq!(layout.input_area.height, 10, "full composer control");
    let status = pixels(f.buffer(), layout.status_bar);
    assert!(row(f.buffer(), layout.status_bar, layout.status_bar.y).contains("Ready"));
    let mut composer = pixels(f.buffer(), layout.input_area);
    for cell in &mut composer {
        cell.modifier.insert(Modifier::DIM);
    }
    let cache = f.cache();
    let view = f.view();
    f.open();
    let parts = tool_inspector::layout::parts(f.frame_rect());
    assert_eq!(parts.body.height, 1);
    assert!(row(f.buffer(), parts.tabs, parts.tabs.y).contains("[Output]"));
    assert!(f.h.screen_text().contains("Wrap on"));
    assert!(f.body_row(0).starts_with('x'));
    assert_ne!(
        last_cell(&f, 0),
        "→",
        "default wraps even in a one-row viewport"
    );
    toggle(&mut f);
    assert_eq!(last_cell(&f, 0), "→");
    assert!(f.h.screen_text().contains("Wrap off"));
    assert!(row(f.buffer(), parts.rail, parts.rail.y).contains("Back"));
    assert!(f.frame_rect().bottom() <= layout.status_bar.y);
    assert_eq!(pixels(f.buffer(), layout.status_bar), status);
    assert_eq!(pixels(f.buffer(), layout.input_area), composer);
    assert_eq!(f.cache(), cache);
    assert_eq!(f.view(), view);
    f.press(Key::Down);
    f.paint();
    assert!(f.body_row(0).starts_with("FIT"));
    assert_ne!(last_cell(&f, 0), "→", "fitting-row control at 60x16");
    assert_eq!(
        f.press(Key::Ctrl('q')),
        InputAction::Quit,
        "global precedence over wrap/viewer"
    );
}

// Preserve the full combined search+wrap lifecycle witness at its Task11 gate.
#[test]
fn nonediting_preemption_restores_query_viewers_wrap_and_painted_matches() {
    use super::k11_search::{commit, marked_cells, output, status};
    let mut f = output(&format!("alpha guard\n{}", rows(50)));
    f.press(Key::Down);
    f.paint();
    f.press(Key::Right);
    f.press(Key::Down);
    f.press(Key::Right);
    f.paint();
    commit(&mut f, "alpha");
    f.press(Key::Char('w'));
    f.paint();
    assert!(status(&f).contains("Wrap off"));
    assert!(status(&f).contains("Search 1/1"));
    assert!(marked_cells(&f) > 0);
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    let viewers = f.tool().viewers;
    let tab = f.tool().tab;
    let cells = pixels(f.buffer(), f.body_rect());
    let viewer_status = status(&f);
    f.clock.advance(Duration::from_secs(2));
    assert_eq!(
        decision_pane::raise(
            &mut f.h.state,
            DecisionDraft::new(DecisionKind::Budget, "nonediting preemption control"),
            Arrival::Automatic
        ),
        Raised::Focused
    );
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::DecisionPane
    );
    assert!(f.h.screen_text().contains("nonediting preemption control"));
    f.clock.advance(decision_pane::INERT_AFTER_PAINT);
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert_eq!(f.tool().tab, tab);
    assert_eq!(f.tool().viewers, viewers);
    assert_eq!(pixels(f.buffer(), f.body_rect()), cells);
    assert_eq!(status(&f), viewer_status);
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn successful_diff_is_reachable_and_copyable_before_the_first_paint() {
    let text = "prepaint-diff".repeat(30);
    let (mut f, id) = Fixture::single(
        80,
        24,
        "Write",
        json!({"content": text}),
        S::Success,
        Some((&text, D::NewFile)),
    );
    f.focus_call(&id);
    assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
    assert!(f.inspect());
    for _ in 0..3 {
        f.press(Key::Right);
    }
    assert_eq!(
        f.press(Key::Char('c')),
        InputAction::CopyToolSource,
        "the decoder defers canonical source resolution to the owning host"
    );
    f.press(Key::Char('w'));
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Diff);
    assert!(f.body_row(0).starts_with("+ prepaint-diff"));
    assert_eq!(last_cell(&f, 0), "→");
    assert!(f.h.screen_text().contains("Wrap off"));
}

#[test]
fn batched_wrap_tab_search_and_navigation_keep_their_input_order() {
    // Run the same real decoder commands with and without intervening paint.
    // Long Output and Input rows make applying a command at the final wrap
    // instead of its input-time wrap observably wrong.
    for first_paint in [false, true] {
        let make = || {
            let text = logical_rows(40);
            let (mut f, id) = Fixture::single(
                80,
                24,
                "Bash",
                json!({"line": text, "items": (0..40).collect::<Vec<_>>()}),
                S::Success,
                Some((&text, D::NotAWrite)),
            );
            f.focus_call(&id);
            assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
            assert!(f.inspect());
            if first_paint {
                f.paint();
            }
            f
        };
        let mut batched = make();
        let mut painted = make();
        let mut keys = vec![
            Key::Down,
            Key::Down,
            Key::Char('w'),
            Key::Down,
            Key::Char('w'),
            Key::Char('/'),
        ];
        keys.extend("line".chars().map(Key::Char));
        keys.extend([
            Key::Enter,
            Key::Char('n'),
            Key::Char('w'),
            Key::Down,
            Key::Right,
            Key::Down,
            Key::Char('w'),
            Key::PageDown,
            Key::Left,
            Key::Char('N'),
            Key::Char('w'),
            Key::Char('n'),
            Key::Char('/'),
            Key::Esc,
            Key::Down,
        ]);
        for key in keys {
            assert_eq!(batched.press(key), painted.press(key));
            painted.paint();
        }
        batched.paint();
        assert!(!batched.tool().wrap, "final requested wrap is restored");
        assert_eq!(batched.tool().tab, ToolTab::Output);
        assert_eq!(batched.tool().wrap, painted.tool().wrap);
        assert_eq!(batched.tool().viewers, painted.tool().viewers);
        assert_eq!(batched.tool().search.query(), painted.tool().search.query());
        assert_eq!(batched.tool().search.focused, painted.tool().search.focused);
        assert!(batched.tool().search.matches.is_empty());
        assert_eq!(
            pixels(batched.buffer(), batched.body_rect()),
            pixels(painted.buffer(), painted.body_rect())
        );
        assert!(batched.body_row(0).starts_with("line"));
        assert_eq!(last_cell(&batched, 0), "→");
    }
}

#[test]
fn unchanged_unwrapped_repaints_do_not_prepare_or_render_the_long_source() {
    let mut f = output(&"x".repeat(1_000_000), 80, 24);
    f.open();
    toggle(&mut f);
    assert_eq!(last_cell(&f, 0), "→");
    metrics::reset();
    tool_inspector::source::metrics::reset();
    for _ in 0..5 {
        f.compose();
    }
    assert_eq!(metrics::renders(), 0);
    assert_eq!(
        tool_inspector::source::metrics::counts(),
        (0, 0),
        "the current revision and cached row widths must gate all payload work"
    );
    assert_eq!(last_cell(&f, 0), "→");
}

#[test]
fn story21_5_unwrapped_real_paints_reuse_render_across_resize() {
    let mut f = output(&logical_rows(40), 80, 24);
    f.open();
    metrics::reset();
    f.press(Key::Char('w'));
    for width in [80, 60, 104] {
        f.h.state.terminal_width = width;
        f.h.terminal.backend_mut().resize(width, 24);
        f.h.terminal.resize(Rect::new(0, 0, width, 24)).unwrap();
        f.compose();
        assert_eq!(last_cell(&f, 0), "→");
    }
    assert_eq!(metrics::renders(), 1);
}

#[test]
fn story21_5_missing_tool_rejects_slash_with_queued_permission() {
    let mut f = output("alpha", 80, 24);
    f.open();
    f.press(Key::Char('/'));
    assert!(f.tool().search.editor.editing, "valid target control");
    f.press(Key::Esc);
    f.clock.advance(Duration::from_secs(2));
    let id = RequestId::new();
    let conversation_id = f.conversation().id.clone();
    approval_bridge::apply(
        &mut f.h.state,
        ApprovalRuntimeEvent::Requested {
            id: id.clone(),
            source: ApprovalSource::ForegroundTurn { conversation_id },
            tool: "Bash".into(),
            risk: ToolRisk::Elevated,
            consent_card: Some("queued missing target".into()),
        },
        None,
    );
    assert_eq!(f.h.state.permission_queue.len(), 1);
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.paint();
    assert!(f.tool().missing);
    let draft = f.h.state.input_buffer.clone();
    assert_eq!(f.press(Key::Char('/')), InputAction::Consumed);
    assert!(!f.tool().search.editor.editing);
    assert_eq!(f.h.state.input_buffer, draft);
    f.press(Key::Esc);
    assert_eq!(
        f.h.state.pending_permission.as_ref().map(|p| &p.id),
        Some(&id)
    );
}
