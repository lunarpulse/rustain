//! K11 / M10: decoder-driven logical search, painted matches and retained owners.
use super::support::*;
use ratatui::{
    buffer::Cell,
    style::{Color, Modifier, Style},
};
use rustain::adapters::tui::{
    app::InputAction,
    decision_pane::{self, Arrival, DecisionDraft, DecisionKind, Raised},
    keymap::{Context, Key},
    presentation::{self, Surface},
    quit_confirm::{self, QuitFacts, QuitHost, QuitRequest},
    tool_inspector::{self, ToolTab},
};
use rustain::domain::{
    models::{
        InvocationStatus as S, RequestId, StreamChunk, ToolRisk, Turn, TurnPart,
        WriteDiffState as D, tool_call::ApprovalSource,
    },
    services::{approval_runtime::ApprovalRuntimeEvent, reducer},
};
use rustain::infrastructure::runtime::approval_bridge;
use serde_json::json;
use std::time::Duration;

pub(super) fn theme(f: &mut Fixture) {
    // Distinct real Theme tokens make an accidental selection/caret insufficient.
    f.h.state.theme.search_highlight = Style::default()
        .fg(Color::Yellow)
        .bg(Color::Blue)
        .add_modifier(Modifier::REVERSED);
    f.h.state.theme.search_highlight_focused = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::REVERSED | Modifier::BOLD);
    f.paint();
}

pub(super) fn output(text: &str) -> Fixture {
    let mut f = Fixture::single(
        80,
        24,
        "Bash",
        json!({"command":"inspect"}),
        S::Success,
        Some((text, D::NotAWrite)),
    )
    .0;
    theme(&mut f);
    f.open();
    f
}

fn context(f: &Fixture) -> Context {
    presentation::frame_context(&f.h.state, f.h.state.modal_stack.top().unwrap().surface)
}

fn type_query(f: &mut Fixture, text: &str) {
    f.press(Key::Char('/'));
    assert_eq!(
        context(f),
        Context::InspectorSearch,
        "slash must lend the shared child editor, not the composer"
    );
    for ch in text.chars() {
        assert_eq!(f.press(Key::Char(ch)), InputAction::Consumed);
    }
}

pub(super) fn commit(f: &mut Fixture, text: &str) {
    type_query(f, text);
    assert_eq!(f.press(Key::Enter), InputAction::Consumed);
    f.paint();
    assert_eq!(context(f), Context::ToolInspector);
}

fn clear(f: &mut Fixture) {
    f.press(Key::Char('/'));
    assert_eq!(context(f), Context::InspectorSearch);
    f.press(Key::Esc);
    f.paint();
    assert_eq!(context(f), Context::ToolInspector);
}

pub(super) fn status(f: &Fixture) -> String {
    let rect = tool_inspector::layout::parts(f.frame_rect())
        .status
        .expect("standard fixture has a real viewer status row");
    row(f.buffer(), rect, rect.y)
}

fn body(f: &Fixture) -> String {
    (0..f.body_rect().height)
        .map(|y| f.body_row(y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn marked(f: &Fixture, cell: &Cell) -> bool {
    [
        f.h.state.theme.search_highlight,
        f.h.state.theme.search_highlight_focused,
    ]
    .into_iter()
    .any(|style| {
        Some(cell.fg) == style.fg
            && Some(cell.bg) == style.bg
            && cell.modifier.contains(style.add_modifier)
    })
}

pub(super) fn marked_cells(f: &Fixture) -> usize {
    pixels(f.buffer(), f.body_rect())
        .iter()
        .filter(|cell| cell.symbol() != " " && marked(f, cell))
        .count()
}

fn mark_at(f: &Fixture, x: u16, y: u16, focused: bool) {
    let rect = f.body_rect();
    let cell = &f.buffer()[(rect.x + x, rect.y + y)];
    let expected = if focused {
        f.h.state.theme.search_highlight_focused
    } else {
        f.h.state.theme.search_highlight
    };
    assert_eq!(Some(cell.fg), expected.fg, "foreground at {x},{y}");
    assert_eq!(Some(cell.bg), expected.bg, "background at {x},{y}");
    assert_eq!(
        cell.style(),
        cell.style().patch(expected),
        "highlight at {x},{y}"
    );
}

fn unmarked_at(f: &Fixture, x: u16, y: u16) {
    let rect = f.body_rect();
    assert!(
        !marked(f, &f.buffer()[(rect.x + x, rect.y + y)]),
        "unmatched production control at {x},{y} must not be highlighted"
    );
}

fn replace_output(f: &mut Fixture, text: &str) {
    let turn = f
        .tabs
        .active_tab_mut()
        .conversation
        .turns
        .last_mut()
        .unwrap();
    let result = turn
        .parts
        .iter_mut()
        .find_map(|part| match part {
            TurnPart::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    result.content = text.into();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
}

fn running(text: &str) -> Fixture {
    let mut f = Fixture::single(80, 22, "Bash", json!({}), S::Success, None).0;
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    reducer::reduce(
        &mut f.tabs.active_tab_mut().reducer,
        StreamChunk::ToolUse {
            id: "wire-search".into(),
            name: "Bash".into(),
            input: json!({"command":"stream"}),
        },
        &*f.clock,
    );
    tail(&mut f, text);
    theme(&mut f);
    f.open();
    f
}

fn tail(f: &mut Fixture, text: &str) {
    let tab = f.tabs.active_tab_mut();
    tab.reducer.set_tail("wire-search", text.into());
    reducer::update_streaming_mirror(&tab.reducer, &mut tab.streaming);
}

#[test]
fn review_r2_5_wrap_reprojection_retains_selected_match() {
    let mut f = output("alpha first\nalpha second\nalpha third\nend");
    commit(&mut f, "alpha");
    f.press(Key::Char('n'));
    f.paint();
    assert!(status(&f).contains("Search 2/3"));
    f.press(Key::Char('w'));
    f.paint();
    assert!(status(&f).contains("Search 2/3"), "{}", status(&f));
    assert_eq!(f.tool().search.focused, Some(1));
}

#[test]
fn review_r2_10_unmatched_and_empty_commits_keep_live_follow() {
    for query in ["absent", ""] {
        let mut f = running(&rows(30));
        let before = f.tool().viewers[0];
        assert!(before.follow);
        commit(&mut f, query);
        assert_eq!(f.tool().viewers[0], before, "query {query:?}");
        assert!(status(&f).contains("Following"));
    }
}

#[test]
fn review_r2_11_non_output_tabs_do_not_claim_running_tool_is_paused() {
    let mut f = running("streaming");
    assert!(status(&f).contains("Following"));
    for tab in [ToolTab::Input, ToolTab::Raw] {
        f.press(Key::Right);
        f.paint();
        assert_eq!(f.tool().tab, tab);
        assert!(!status(&f).contains("Paused"), "{tab:?}: {}", status(&f));
    }
}

#[test]
fn commit_jumps_to_the_first_match_and_stops_following() {
    let mut lines = (0..16)
        .map(|n| format!("stream {n:02}"))
        .collect::<Vec<_>>();
    lines[1] = "alpha guard".into();
    let mut f = running(&lines.join("\n"));
    assert!(f.tool().viewers[0].follow);
    assert!(status(&f).contains("Following"));
    let before = f.tool().viewers[0].top;
    assert!(
        before > 2,
        "control: the real following tail is below the only match"
    );
    commit(&mut f, "ALPHA");
    assert_eq!(
        f.tool().viewers[0].top,
        2,
        "one live-tail header precedes logical row 1"
    );
    assert!(f.body_row(0).starts_with("alpha guard"));
    assert!(
        !f.tool().viewers[0].follow,
        "M10: committing must pause follow, not just move top"
    );
    assert!(status(&f).contains("Paused") && status(&f).contains("Search 1/1"));
    for x in 0..5 {
        mark_at(&f, x, 0, true);
    }
    unmarked_at(&f, 6, 0);
    let first = f.body_row(0);
    lines.push("later tail growth".into());
    tail(&mut f, &lines.join("\n"));
    f.paint();
    assert_eq!(f.tool().viewers[0].top, 2);
    assert_eq!(
        f.body_row(0),
        first,
        "canonical growth must not resume following"
    );
    assert!(status(&f).contains("Paused"));
    // A fresh opening has no prepared body. Its initial following tail must be
    // established before the pending commit pauses it (the M10 owning seam).
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
    assert!(f.inspect());
    assert!(f.tool().rows().is_empty());
    assert!(f.tool().viewers[0].follow);
    commit(&mut f, "alpha");
    assert_eq!(f.tool().viewers[0].top, 2);
    assert!(f.body_row(0).starts_with("alpha guard"));
    assert!(
        !f.tool().viewers[0].follow,
        "unpainted commit must pause the real initial tail"
    );
    assert!(status(&f).contains("Paused") && status(&f).contains("Search 1/1"));
}

#[test]
fn next_previous_wrap_and_first_match_at_or_after_the_existing_top() {
    let mut f = output(&rows(100));
    let width = usize::from(f.body_rect().width);
    let mut lines = (0..100).map(|n| format!("row {n:03}")).collect::<Vec<_>>();
    for n in [3, 40, 77] {
        lines[n] = format!("alpha row {n:03}");
    }
    lines[60] = format!("{}ALPHA", "x".repeat(width - 2));
    replace_output(&mut f, &lines.join("\n"));
    f.paint();
    assert_eq!(marked_cells(&f), 0, "unsearched text is the paint control");
    commit(&mut f, "aLpHa");
    for (index, top, label) in [
        (1, 3, "alpha row 003"),
        (2, 40, "alpha row 040"),
        (3, 60, "xx"),
        (4, 78, "alpha row 077"),
        (1, 3, "alpha row 003"),
    ] {
        assert_eq!(f.tool().viewers[0].top, top);
        assert!(f.body_row(0).contains(label));
        assert!(
            status(&f).contains(&format!("Search {index}/4")),
            "{}",
            status(&f)
        );
        if index == 3 {
            assert!(f.body_row(0).ends_with("AL"));
            assert!(f.body_row(1).starts_with("PHA"));
            for x in [width as u16 - 2, width as u16 - 1] {
                mark_at(&f, x, 0, true);
            }
            for x in 0..3 {
                mark_at(&f, x, 1, true);
            }
            unmarked_at(&f, 0, 0);
        }
        f.press(Key::Char('n'));
        f.paint();
    }
    // The loop's last n selected #2; N returns to #1 and then wraps to #4.
    for index in [1, 4] {
        f.press(Key::Char('N'));
        f.paint();
        assert!(status(&f).contains(&format!("Search {index}/4")));
    }
    assert!(f.body_row(0).starts_with("alpha row 077"));
    clear(&mut f);
    f.press(Key::Char('g'));
    for _ in 0..10 {
        f.press(Key::Down);
    }
    f.paint();
    assert_eq!(f.tool().viewers[0].top, 10);
    commit(&mut f, "alpha");
    assert_eq!(f.tool().viewers[0].top, 40);
    assert!(f.body_row(0).starts_with("alpha row 040"));
    assert!(status(&f).contains("Search 2/4"));
    clear(&mut f);
    let top = f.tool().viewers[0].top;
    let before = pixels(f.buffer(), f.body_rect());
    commit(&mut f, "not-present");
    assert!(status(&f).contains("Search: no match for \"not-present\""));
    for key in [Key::Char('n'), Key::Char('N')] {
        f.press(key);
        f.paint();
        assert_eq!(f.tool().viewers[0].top, top);
        assert_eq!(pixels(f.buffer(), f.body_rect()), before);
        assert!(!status(&f).contains("Search 1/"));
    }
}

#[test]
fn substring_styles_distinguish_focused_unfocused_and_unmatched_text() {
    let mut f = output("guard preALPHAsuffix / alpha end\nquiet control");
    assert_eq!(marked_cells(&f), 0);
    commit(&mut f, "alpha");
    assert!(status(&f).contains("Search 1/2"));
    for x in 9..14 {
        mark_at(&f, x, 0, true);
    }
    for x in 23..28 {
        mark_at(&f, x, 0, false);
    }
    unmarked_at(&f, 0, 0);
    unmarked_at(&f, 14, 0);
    unmarked_at(&f, 0, 1);
    f.press(Key::Char('n'));
    f.paint();
    assert!(status(&f).contains("Search 2/2"));
    for x in 9..14 {
        mark_at(&f, x, 0, false);
    }
    for x in 23..28 {
        mark_at(&f, x, 0, true);
    }
}

#[test]
fn unicode_caseless_sigma_expansion_and_grapheme_cells_have_real_highlights() {
    for (source, query, count, cells) in [
        ("guard Σ / σ / ς end", "σ", 3, 3),
        ("guard ΟΣ / ος end", "οσ", 2, 4),
        ("guard İ end", "\u{307}", 1, 1),
        (
            "guard 界e\u{301}👩\u{200d}💻 end",
            "界e\u{301}👩\u{200d}💻",
            1,
            3,
        ),
    ] {
        let mut f = output(source);
        assert!(body(&f).contains("guard"));
        assert_eq!(marked_cells(&f), 0);
        commit(&mut f, query);
        assert!(
            status(&f).contains(&format!("Search 1/{count}")),
            "{source:?}: {}",
            status(&f)
        );
        assert_eq!(
            marked_cells(&f),
            cells,
            "{source:?}: count painted grapheme starts, not UTF-8 bytes"
        );
        unmarked_at(&f, 0, 0);
        if query.starts_with('界') {
            let rect = f.body_rect();
            for (x, symbol) in [(6, "界"), (8, "e\u{301}"), (9, "👩\u{200d}💻")] {
                assert_eq!(f.buffer()[(rect.x + x, rect.y)].symbol(), symbol);
                mark_at(&f, x, 0, true);
            }
            unmarked_at(&f, 11, 0);
        }
        for index in 2..=count {
            f.press(Key::Char('n'));
            f.paint();
            assert!(status(&f).contains(&format!("Search {index}/{count}")));
            assert_eq!(marked_cells(&f), cells);
        }
    }
    let mut f = output("guard e\u{301} end");
    commit(&mut f, "é");
    assert!(
        status(&f).contains("Search: no match for \"é\""),
        "caseless is not normalization"
    );
    assert_eq!(marked_cells(&f), 0);
    clear(&mut f);
    commit(&mut f, "e\u{301}");
    assert!(status(&f).contains("Search 1/1"));
    mark_at(&f, 6, 0, true);
    unmarked_at(&f, 7, 0);
}

#[test]
fn a_phrase_crossing_a_real_soft_wrap_is_highlighted_on_both_rows() {
    let mut f = output("placeholder");
    let width = usize::from(f.body_rect().width);
    // Leave exactly enough room for ALPHA, but not the next word. This
    // exercises a phrase across a word-boundary wrap, not a source newline.
    let text = format!("{}ALPHA beta guard\nquiet control", "x".repeat(width - 5));
    replace_output(&mut f, &text);
    f.paint();
    assert!(f.body_row(0).ends_with("ALPHA"));
    assert!(f.body_row(1).starts_with("beta guard"));
    assert_eq!(marked_cells(&f), 0);
    commit(&mut f, "alpha beta");
    assert!(status(&f).contains("Search 1/1"));
    for x in width as u16 - 5..width as u16 {
        mark_at(&f, x, 0, true);
    }
    for x in 0..4 {
        mark_at(&f, x, 1, true);
    }
    unmarked_at(&f, 0, 0);
    unmarked_at(&f, 5, 1);
    clear(&mut f);
    assert_eq!(marked_cells(&f), 0);
    assert!(f.body_row(0).ends_with("ALPHA") && f.body_row(1).starts_with("beta guard"));
}

#[test]
fn sanitized_logical_lines_preserve_crlf_and_bare_cr_without_cross_line_matches() {
    let mut f = output(
        "al\x1b[31mPHA\x1b[0m guard\r\nalpha guard\ralpha guard\n\
        al\x07pha guard\n\x1b]0;hidden-needle\x07quiet control",
    );
    assert!(f.body_row(0).starts_with("alPHA guard"));
    assert!(f.body_row(1).starts_with("alpha guard"));
    assert!(f.body_row(2).starts_with("alpha guard"));
    assert!(f.body_row(3).starts_with("alpha guard"));
    commit(&mut f, "alpha");
    assert!(status(&f).contains("Search 1/4"));
    for y in 0..4 {
        for x in 0..5 {
            mark_at(&f, x, y, y == 0);
        }
        unmarked_at(&f, 6, y);
    }
    clear(&mut f);
    for query in ["guardalpha", "guard alpha", "hidden-needle", "31m"] {
        commit(&mut f, query);
        assert!(status(&f).contains(&format!("Search: no match for \"{query}\"")));
        assert_eq!(marked_cells(&f), 0);
        assert_eq!(f.tool().viewers[0].top, 0);
        clear(&mut f);
    }
}

#[test]
fn field_grapheme_edits_and_escape_never_touch_the_stage_or_full_composer() {
    for (width, height, full) in [(80, 24, false), (60, 16, true)] {
        let mut f = Fixture::single(
            width,
            height,
            "Bash",
            json!({"command":"inspect"}),
            S::Success,
            Some(("guard 界x end\nquiet control", D::NotAWrite)),
        )
        .0;
        f.h.state.input_buffer = if full {
            ["draft text"; 8].join("\n")
        } else {
            "draft text".into()
        };
        f.h.state.cursor_position = 3;
        theme(&mut f);
        let layout = f.layout();
        let stage = pixels(f.buffer(), layout.chat_pane);
        let composer = pixels(f.buffer(), layout.input_area);
        let view = f.view();
        let cache = f.cache();
        let transcript = serde_json::to_vec(f.conversation()).unwrap();
        let draft = (
            f.h.state.input_buffer.clone(),
            f.h.state.cursor_position,
            f.h.state.input_scroll_offset,
        );
        let focused = f.h.state.focused_tool_id.clone();
        f.open();
        let frame = f.h.state.modal_stack.top().unwrap().clone();
        let top = f.tool().viewers[0].top;
        type_query(&mut f, "qj");
        f.paint();
        assert_eq!(f.tool().viewers[0].top, top, "j belongs to the field");
        assert_eq!(
            marked_cells(&f),
            0,
            "typing must not commit body highlights"
        );
        assert!(
            f.h.screen_text().contains("qj"),
            "the actual child query must paint"
        );
        f.press(Key::Esc);
        f.paint();
        assert_eq!(
            f.h.state.modal_stack.top(),
            Some(&frame),
            "first Escape exits only the child"
        );
        assert_eq!(context(&f), Context::ToolInspector);
        assert!(!f.h.screen_text().contains("qj"));
        type_query(&mut f, "界e\u{301}x");
        f.press(Key::Left);
        f.press(Key::Backspace); // Remove the entire combining grapheme, not just its accent.
        f.press(Key::End);
        f.press(Key::Enter);
        f.paint();
        assert_eq!(context(&f), Context::ToolInspector);
        assert!(f.body_row(0).starts_with("guard "));
        let rect = f.body_rect();
        assert_eq!(f.buffer()[(rect.x + 6, rect.y)].symbol(), "界");
        assert_eq!(
            f.buffer()[(rect.x + 7, rect.y)].symbol(),
            " ",
            "wide continuation cell"
        );
        assert_eq!(f.buffer()[(rect.x + 8, rect.y)].symbol(), "x");
        assert!(f.body_row(0).contains("x end"));
        mark_at(&f, 6, 0, true);
        mark_at(&f, 8, 0, true);
        unmarked_at(&f, 9, 0);
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
        let now = f.layout();
        assert_eq!(
            (now.chat_pane, now.input_area, now.status_bar),
            (layout.chat_pane, layout.input_area, layout.status_bar)
        );
        assert!(row(f.buffer(), now.status_bar, now.status_bar.y).contains("Ready"));
        assert!(f.body_rect().height >= 1);
        clear(&mut f);
        assert_eq!(
            marked_cells(&f),
            0,
            "field Escape clears committed highlights too"
        );
        f.press(Key::Esc);
        f.paint();
        assert!(f.h.state.modal_stack.is_empty());
        assert_eq!(f.h.state.focused_tool_id, focused);
        assert_eq!(pixels(f.buffer(), layout.chat_pane), stage);
        assert_eq!(pixels(f.buffer(), layout.input_area), composer);
        assert_eq!(f.view(), view);
        assert_eq!(f.cache(), cache);
    }
}

#[test]
fn actual_tab_switches_and_canonical_source_changes_recompute_matches() {
    let (mut f, id) = Fixture::single(
        80,
        24,
        "Write",
        json!({"file_path":"notes.txt", "content":"alpha\nalpha"}),
        S::Success,
        Some(("alpha guard", D::NewFile)),
    );
    theme(&mut f);
    f.open();
    commit(&mut f, "alpha");
    assert!(status(&f).contains("Search 1/1"));
    for (tab, count) in [(ToolTab::Input, 2), (ToolTab::Raw, 3), (ToolTab::Diff, 2)] {
        f.press(Key::Right);
        f.paint();
        assert_eq!(f.tool().tab, tab);
        assert!(
            status(&f).contains(&format!("Search 1/{count}")),
            "{tab:?}: {}",
            status(&f)
        );
        assert_eq!(marked_cells(&f), count * 5);
    }
    assert!(
        body(&f).contains("+ alpha"),
        "Diff is a real completed Write source"
    );
    f.press(Key::Left);
    f.press(Key::Left);
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Input);
    if let TurnPart::ToolInvocation { args, .. } = f.invocation_mut(&id) {
        args["content"] = json!("quiet control");
    }
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.paint();
    assert!(
        body(&f).contains("quiet control"),
        "canonical argument mutation reaches actual cells"
    );
    assert!(status(&f).contains("Search: no match for \"alpha\""));
    assert_eq!(marked_cells(&f), 0);
    f.press(Key::Right);
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Raw);
    assert!(
        status(&f).contains("Search 1/1"),
        "Raw retains only the independent result occurrence"
    );
    assert_eq!(marked_cells(&f), 5);
    f.press(Key::Right);
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Diff);
    assert!(body(&f).contains("+ quiet control"));
    assert!(status(&f).contains("Search: no match for \"alpha\""));
    assert_eq!(marked_cells(&f), 0);
    for _ in 0..3 {
        f.press(Key::Left);
    }
    replace_output(&mut f, "alpha guard\nALPHA guard\nquiet control");
    f.paint();
    assert!(status(&f).contains("Search 1/2"));
    assert_eq!(marked_cells(&f), 10);
}

#[test]
fn live_tail_replacement_and_completion_recompute_without_retyping() {
    let mut f = running("alpha guard\nquiet control");
    commit(&mut f, "alpha");
    assert!(status(&f).contains("Search 1/1"));
    tail(&mut f, "quiet control\nquiet control");
    f.paint();
    assert!(body(&f).contains("quiet control"));
    assert!(status(&f).contains("Search: no match for \"alpha\""));
    assert_eq!(marked_cells(&f), 0);
    reducer::reduce(
        &mut f.tabs.active_tab_mut().reducer,
        StreamChunk::ToolResult {
            id: "wire-search".into(),
            operator_private: false,
            content: "alpha guard\nALPHA guard\nquiet control".into(),
            is_error: false,
            diff: D::NotAWrite,
            images: vec![],
        },
        &*f.clock,
    );
    f.paint();
    assert!(status(&f).contains("Search 1/2"));
    assert_eq!(marked_cells(&f), 10);
    assert!(!status(&f).contains("Following") && !status(&f).contains("Paused"));
}

#[test]
fn same_turn_step_clears_the_query_instead_of_reusing_another_calls_matches() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let first = call(
        &mut turn,
        "Bash",
        json!({"command":"first"}),
        S::Success,
        Some(("alpha first guard", D::NotAWrite)),
    );
    call(
        &mut turn,
        "Bash",
        json!({"command":"second"}),
        S::Success,
        Some(("alpha second guard", D::NotAWrite)),
    );
    let mut f = Fixture::new(80, 24, turn);
    theme(&mut f);
    f.focus_call(&first);
    f.open();
    commit(&mut f, "alpha");
    assert!(status(&f).contains("Search 1/1") && marked_cells(&f) == 5);
    let frame_id = f.h.state.modal_stack.top().unwrap().id;
    assert!(
        f.step(Key::Char(']')),
        "the real host effect must retarget the frame"
    );
    f.paint();
    assert_eq!(f.h.state.modal_stack.top().unwrap().id, frame_id);
    assert!(f.body_row(0).contains("alpha second guard"));
    assert!(!status(&f).contains("Search"));
    assert_eq!(marked_cells(&f), 0);
    f.press(Key::Char('n'));
    f.paint();
    assert_eq!(marked_cells(&f), 0);
    commit(&mut f, "alpha");
    assert!(status(&f).contains("Search 1/1") && marked_cells(&f) == 5);
    assert!(
        f.step(Key::Char('[')),
        "return stepping uses the same canonical host effect"
    );
    f.paint();
    assert!(f.body_row(0).contains("alpha first guard"));
    assert!(!status(&f).contains("Search"));
    assert_eq!(marked_cells(&f), 0);
}

#[test]
fn queued_permission_and_automatic_arrival_cannot_steal_an_idle_search_field() {
    let mut f = output("alpha guard\nquiet control");
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    type_query(&mut f, "alpha");
    f.paint();
    // Exceed both the 500ms rail guard and recent-printable grace. TextEntry,
    // not a clock accident, must protect the editor even with a real queue item.
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
            consent_card: Some("search field permission control".into()),
        },
        None,
    );
    assert!(f.h.state.pending_permission.is_none());
    assert_eq!(f.h.state.permission_queue.len(), 1);
    assert_eq!(
        decision_pane::raise(
            &mut f.h.state,
            DecisionDraft::new(DecisionKind::Budget, "waiting field arrival"),
            Arrival::Automatic
        ),
        Raised::Waiting
    );
    f.clock.advance(Duration::from_millis(501));
    f.paint();
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert_eq!(context(&f), Context::InspectorSearch);
    assert!(f.h.state.pending_permission.is_none());
    assert_eq!(f.h.state.permission_queue.len(), 1);
    assert!(
        !f.h.screen_text()
            .contains("search field permission control")
    );
    f.press(Key::Enter);
    f.paint();
    assert!(
        status(&f).contains("Search 1/1"),
        "the retained actual query still commits"
    );
    assert_eq!(marked_cells(&f), 5);
    f.press(Key::Esc);
    assert_eq!(
        f.h.state.pending_permission.as_ref().map(|p| &p.id),
        Some(&id),
        "production close promotes the exact real permission; it was not dropped"
    );
    f.paint();
    assert!(
        f.h.screen_text()
            .contains("search field permission control")
    );
}

#[test]
fn nonediting_preemption_restores_query_viewers_and_painted_matches() {
    let mut f = output(&format!("alpha guard\n{}", rows(50)));
    f.press(Key::Down);
    f.paint();
    f.press(Key::Right);
    f.press(Key::Down);
    f.press(Key::Right);
    f.paint();
    commit(&mut f, "alpha");
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
fn ctrl_q_precedes_the_field_and_cancel_restores_its_real_query() {
    let mut f = output("alpha guard\nquiet control");
    type_query(&mut f, "alpha");
    f.paint();
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    assert_eq!(f.press(Key::Ctrl('q')), InputAction::Quit);
    assert_eq!(
        quit_confirm::request(
            &mut f.h.state,
            QuitFacts {
                streams: 1,
                ..QuitFacts::idle(QuitHost::Local)
            }
        ),
        QuitRequest::Confirming
    );
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::QuitConfirm
    );
    assert!(presentation::is_open(&f.h.state, frame.surface));
    f.press(Key::Esc);
    f.paint();
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert_eq!(context(&f), Context::InspectorSearch);
    f.press(Key::Enter);
    f.paint();
    assert!(status(&f).contains("Search 1/1"));
    assert_eq!(marked_cells(&f), 5);
    assert!(presentation::consistent(&f.h.state));
}

#[test]
fn missing_target_exits_editor_ownership_and_back_releases_the_real_queued_permission() {
    let mut f = output("alpha target");
    let draft = (f.h.state.input_buffer.clone(), f.h.state.cursor_position);
    type_query(&mut f, "query-only");
    f.paint();
    assert_eq!(context(&f), Context::InspectorSearch);
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
            consent_card: Some("missing editor permission control".into()),
        },
        None,
    );
    assert!(f.h.state.pending_permission.is_none());
    assert_eq!(f.h.state.permission_queue.len(), 1);
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.paint();
    assert!(
        f.h.screen_text()
            .contains("This tool call is no longer in the conversation.")
    );
    assert_eq!(
        context(&f),
        Context::ToolInspector,
        "A16: missing targets lend no editor"
    );
    assert!(!f.h.screen_text().contains("query-only"));
    assert_eq!(f.press(Key::Char('j')), InputAction::Consumed);
    assert_eq!(
        (f.h.state.input_buffer.clone(), f.h.state.cursor_position),
        draft
    );
    f.press(Key::Esc);
    assert!(
        f.h.state.tool_inspectors.is_empty(),
        "one Back releases the missing owner"
    );
    assert_eq!(
        f.h.state.pending_permission.as_ref().map(|p| &p.id),
        Some(&id)
    );
    f.paint();
    assert!(
        f.h.screen_text()
            .contains("missing editor permission control")
    );
    assert!(presentation::consistent(&f.h.state));
}
