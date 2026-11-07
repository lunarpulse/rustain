//! K3 (AC3, AC4): reasoning is reachable. Tab/J/K reach it like tools and
//! blocks, `Enter` opens the S-BLOCK frame titled `Thinking · …`, copy returns
//! the original bytes, closing leaves the Stage untouched, and the live row
//! shows while the model thinks (local and attached).
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::tui::{
    app::InputAction,
    keymap::Key,
    presentation::{
        self, Surface,
        markdown_body::{self, BodyMode},
    },
    state::TabRenderState,
    theme::Theme,
    widgets::chat_pane,
};
use rustain::domain::models::{
    InvocationStatus, StreamChunk, StreamingState, Turn, WriteDiffState, turn::block_item_id_for,
    view_state::ViewState,
};
use rustain::domain::services::peer_text::sanitize_tool_text_block;
use rustain::domain::services::reducer::{reduce, update_streaming_mirror};

use super::support::{
    Fixture, block, body_row, call, code, copy, focus, open, open_focused, part, parts, rail_row,
    reasoning, row_starting, rows, status_row, tab,
};

/// Search term once, a wide code row, and a raw ESC the display never shows.
fn think_text() -> String {
    format!(
        "Consider the beta option first.\n\nThen a plan:\n\n```rust\nlet wide = \"{}END\";\n```\n\nColor \x1b[31mred\x1b[0m stays safe.",
        "w".repeat(110)
    )
}

struct Ids {
    reasoning: String,
    tool: String,
    block: String,
}

fn fixture(width: u16, height: u16) -> (Fixture, Ids) {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let reasoning_id = reasoning(&mut turn, &think_text(), Some(4_200), Some(1_300));
    let tool = call(
        &mut turn,
        "Bash",
        serde_json::json!({"command": "ls"}),
        InvocationStatus::Success,
        Some(("output", WriteDiffState::NotAWrite)),
    );
    let prose = part(&mut turn, &format!("Answer.\n\n{}", code(3)));
    let block = block_item_id_for(&turn.id, prose, 0);
    let f = Fixture::new(width, height, turn);
    (
        f,
        Ids {
            reasoning: reasoning_id,
            tool,
            block,
        },
    )
}

fn title(f: &Fixture) -> String {
    f.h.state.modal_stack.top().unwrap().title().to_owned()
}

fn press(f: &mut Fixture, keys: &[Key]) {
    for key in keys {
        assert_eq!(f.press(*key), InputAction::Consumed, "{key:?}");
    }
    f.compose();
}

fn layout_text(cols: usize, mode: BodyMode) -> Vec<String> {
    markdown_body::layout(
        &sanitize_tool_text_block(&think_text()),
        cols,
        &Theme::dark(),
        mode,
        false,
    )
    .lines
    .iter()
    .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
    .collect()
}

fn frame_rows(f: &Fixture) -> Vec<String> {
    block(f)
        .rows
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn tab_reaches_reasoning_like_tools_and_blocks() {
    let (mut f, ids) = fixture(80, 40);
    focus(&mut f, &ids.reasoning);
    assert_eq!(tab(&mut f).as_deref(), Some(ids.tool.as_str()));
    assert_eq!(tab(&mut f).as_deref(), Some(ids.block.as_str()));
    assert_eq!(
        tab(&mut f).as_deref(),
        Some(ids.reasoning.as_str()),
        "the cycle wraps back to the thinking row"
    );
}

#[test]
fn the_focused_row_shows_the_marker_and_the_generated_cue() {
    let (mut f, ids) = fixture(80, 40);
    focus(&mut f, &ids.tool);
    let unfocused = rows(&f);
    assert!(
        unfocused
            .iter()
            .any(|r| r == "│ ◇ Thinking · 4.2s · 1.3k tokens"),
        "control: another item focused leaves the glyph: {unfocused:?}"
    );
    focus(&mut f, &ids.reasoning);
    let focused = rows(&f);
    assert!(
        focused
            .iter()
            .any(|r| r == "│ ▸ Thinking · 4.2s · 1.3k tokens [Enter] Inspect"),
        "{focused:?}"
    );
}

#[test]
fn enter_opens_the_thinking_window_with_the_reasoning_body() {
    let (mut f, ids) = fixture(80, 40);
    open(&mut f, &ids.reasoning);
    f.compose();
    assert!(matches!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::BlockViewer(_)
    ));
    assert_eq!(title(&f), "Thinking · 4.2s · 1.3k tokens");
    let cols = usize::from(parts(&f).body.width);
    assert_eq!(frame_rows(&f), layout_text(cols, BodyMode::Rendered));
    let joined = frame_rows(&f).join("\n");
    assert!(joined.contains("Color red stays safe."), "controls dropped");
    assert!(!joined.contains('\u{1b}'));
    assert!(
        status_row(&f).starts_with("thinking · ") && status_row(&f).contains("Rendered"),
        "{}",
        status_row(&f)
    );
    // The body rows are what the frame paints.
    assert!(body_row(&f, 0).starts_with("Consider the beta option first."));
    assert_eq!(
        f.h.state.selected_item_id.as_deref(),
        Some(ids.reasoning.as_str())
    );
}

#[test]
fn v_switches_between_rendered_and_the_sanitized_source() {
    let (mut f, ids) = fixture(80, 40);
    open(&mut f, &ids.reasoning);
    let cols = usize::from(parts(&f).body.width);
    press(&mut f, &[Key::Char('v')]);
    assert_eq!(frame_rows(&f), layout_text(cols, BodyMode::Source));
    assert!(
        frame_rows(&f)[4].starts_with("```rust"),
        "source shows the fence"
    );
    press(&mut f, &[Key::Char('v')]);
    assert_eq!(frame_rows(&f), layout_text(cols, BodyMode::Rendered));
}

#[test]
fn horizontal_keys_move_only_the_wide_rows() {
    let (mut f, ids) = fixture(80, 40);
    open(&mut f, &ids.reasoning);
    f.compose();
    press(&mut f, &[Key::Char('$')]);
    let state = block(&f);
    assert!(state.viewer.h_off > 0, "the wide code row can scroll");
    let rows = frame_rows(&f);
    let prose = rows.iter().position(|r| r.contains("Consider")).unwrap();
    let wide = rows.iter().position(|r| r.contains("wwww")).unwrap();
    assert_eq!(state.row_h_off(prose), 0, "prose never moves");
    assert_eq!(state.row_h_off(wide), state.viewer.h_off);
    assert!(
        body_row(&f, u16::try_from(prose - state.viewer.top).unwrap())
            .starts_with("Consider the beta option first."),
        "the painted prose row did not shift"
    );
    // Control: a reasoning text with no wide row leaves `$` with nothing to do.
    let mut turn = Turn::new("fixture-model".into(), 0);
    let id = reasoning(&mut turn, "short thought", Some(1_000), Some(3));
    part(&mut turn, "Answer.");
    let mut g = Fixture::new(80, 24, turn);
    open(&mut g, &id);
    press(&mut g, &[Key::Char('$')]);
    assert_eq!(block(&g).viewer.h_off, 0);
}

#[test]
fn keys_that_do_not_apply_do_nothing_and_are_not_painted() {
    let (mut f, ids) = fixture(80, 40);
    open(&mut f, &ids.reasoning);
    f.compose();
    let before = (block(&f).numbers, block(&f).wrap, block(&f).target.clone());
    press(&mut f, &[Key::Char('#'), Key::Char('w')]);
    for key in [Key::Char('['), Key::Char(']')] {
        let InputAction::StepBlock { forward } = f.press(key) else {
            panic!("{key:?} decodes to the block step request");
        };
        let tab = f.tabs.active_tab();
        assert!(
            !rustain::adapters::tui::block_viewer::step(
                &mut f.h.state,
                &tab.conversation,
                tab.reducer.open_turn.as_ref(),
                forward,
            ),
            "{key:?}: a reasoning window has no neighbours to step to"
        );
    }
    f.compose();
    assert_eq!(
        (block(&f).numbers, block(&f).wrap, block(&f).target.clone()),
        before
    );
    let rail = rail_row(&f);
    for label in ["Numbers", "Wrap", "Previous", "Next"] {
        assert!(!rail.contains(label), "{label} must not be painted: {rail}");
    }
    assert!(rail.contains("Search"), "applicable rows stay: {rail}");
    // Positive control: the same keys act on a code block frame.
    press(&mut f, &[Key::Esc]);
    open(&mut f, &ids.block);
    f.compose();
    assert!(rail_row(&f).contains("Numbers") && rail_row(&f).contains("Wrap"));
    press(&mut f, &[Key::Char('#'), Key::Char('w')]);
    assert!(block(&f).numbers && block(&f).wrap);
}

#[test]
fn search_finds_text_the_folded_stage_never_shows() {
    let (mut f, ids) = fixture(80, 40);
    assert!(
        rows(&f).iter().all(|r| !r.contains("beta")),
        "the fold hides the text"
    );
    open(&mut f, &ids.reasoning);
    f.compose();
    press(&mut f, &[Key::Char('/')]);
    for ch in "beta".chars() {
        press(&mut f, &[Key::Char(ch)]);
    }
    press(&mut f, &[Key::Enter]);
    assert!(
        status_row(&f).starts_with("Search 1/1 · "),
        "{}",
        status_row(&f)
    );
    assert!(body_row(&f, 0).contains("beta"));
}

#[test]
fn copy_returns_the_original_reasoning_bytes() {
    let (mut f, ids) = fixture(80, 40);
    open(&mut f, &ids.reasoning);
    f.compose();
    let copied = copy(&mut f, Key::Char('c'));
    assert_eq!(copied, [think_text()], "frame `c`: raw ESC and all");
    assert!(copied[0].contains('\u{1b}'));
    // Chat `c` with the reasoning item selected copies the same bytes.
    press(&mut f, &[Key::Esc]);
    assert_eq!(
        f.h.state.selected_item_id.as_deref(),
        Some(ids.reasoning.as_str())
    );
    assert_eq!(copy(&mut f, Key::Char('c')), [think_text()]);
}

/// Everything the Stage and composer own (the 21.13 K6 witness set).
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

#[test]
fn closing_the_window_leaves_stage_caches_viewstate_and_composer_identical() {
    let (mut f, ids) = fixture(80, 40);
    f.h.state.input_buffer = "retained draft".into();
    f.h.state.cursor_position = 4;
    focus(&mut f, &ids.reasoning);
    f.paint();
    let before = stage(&f);
    open_focused(&mut f);
    assert_eq!(stage(&f), before, "opening edits nothing");
    press(
        &mut f,
        &[
            Key::Char('j'),
            Key::Char('$'),
            Key::Char('v'),
            Key::Char('/'),
            Key::Char('x'),
            Key::Enter,
        ],
    );
    assert_eq!(
        stage(&f),
        before,
        "scrolling, toggling and searching stay in the frame"
    );
    press(&mut f, &[Key::Esc]);
    f.paint();
    assert!(f.h.state.modal_stack.is_empty() && f.h.state.block_viewers.is_empty());
    assert_eq!(stage(&f), before);
    assert!(presentation::consistent(&f.h.state));
}

fn attached(
    conversation: &rustain::domain::models::Conversation,
    streaming: &StreamingState,
    width: u16,
    height: u16,
) -> (Vec<String>, TabRenderState) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut render = TabRenderState::default();
    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render_attached(
                frame,
                area,
                conversation,
                streaming,
                &ViewState::default(),
                &Theme::dark(),
                &mut render,
                &BTreeMap::new(),
                &HashMap::new(),
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    (rows, render)
}

#[test]
fn the_attached_host_paints_header_and_body_and_offers_no_reasoning_item() {
    let (f, _) = fixture(80, 40);
    let conversation = f.conversation().clone();
    let (rows, render) = attached(&conversation, &StreamingState::default(), 80, 40);
    let at = row_starting(&rows, "│ ◇ Thinking").expect("header painted");
    assert_eq!(rows[at], "│ ◇ Thinking · 4.2s · 1.3k tokens");
    assert_eq!(rows[at + 1], "│ Consider the beta option first.");
    assert!(
        render
            .item_boundaries
            .iter()
            .all(|(_, id)| !id.starts_with("rs_")),
        "no Stage focus in the attached host: {:?}",
        render.item_boundaries
    );
    // Control: the local host does list it.
    let (mut local, ids) = fixture(80, 40);
    local.paint();
    let boundaries = &local.h.state.tab_render_states[&local.tabs.active_tab_id()].item_boundaries;
    assert!(
        boundaries.iter().any(|(_, id)| *id == ids.reasoning),
        "{boundaries:?}"
    );
}

fn think_chunk(text: &str) -> StreamChunk {
    StreamChunk::Thinking {
        content: text.into(),
        parent_tool_use_id: None,
    }
}

#[test]
fn the_live_row_shows_while_thinking_and_becomes_the_committed_row_in_place() {
    let turn = Turn::new("fixture-model".into(), 0);
    let mut f = Fixture::new(80, 24, turn);
    f.tabs.active_tab_mut().view_state.mode = rustain::domain::models::AnchorMode::Following;
    f.h.state.set_auto_scroll(true);
    // Nothing thinks yet: no live row, no stray rail.
    assert!(rows(&f).iter().all(|r| !r.contains("Thinking")));
    {
        let tab = f.tabs.active_tab_mut();
        let clock = tab.clock.clone();
        reduce(&mut tab.reducer, think_chunk(&"a".repeat(480)), &*clock);
        update_streaming_mirror(&tab.reducer, &mut tab.streaming);
        let started = tab.streaming.thinking_started_ms.expect("start recorded");
        f.clock.set_wall_anchor_ms(started);
    }
    f.clock.advance(Duration::from_millis(3_100));
    f.paint();
    let live = rows(&f);
    let at = row_starting(&live, "│ ◇ Thinking…").expect("live row");
    assert_eq!(live[at], "│ ◇ Thinking… · 3.1s · ~120 tokens");
    assert_eq!(live[at + 1], "│", "the open turn keeps its rail");
    let height = f.h.state.total_content_height;
    // Elapsed advances with the clock.
    f.clock.advance(Duration::from_millis(1_000));
    f.paint();
    assert_eq!(
        row_starting(&rows(&f), "│ ◇ Thinking…").map(|i| rows(&f)[i].clone()),
        Some("│ ◇ Thinking… · 4.1s · ~120 tokens".to_owned())
    );
    // The part commits (an empty answer chunk flushes it): same row count.
    {
        let tab = f.tabs.active_tab_mut();
        let clock = tab.clock.clone();
        reduce(
            &mut tab.reducer,
            StreamChunk::Text {
                content: String::new(),
                parent_tool_use_id: None,
            },
            &*clock,
        );
        update_streaming_mirror(&tab.reducer, &mut tab.streaming);
    }
    f.paint();
    let committed = rows(&f);
    let at = row_starting(&committed, "│ ◇ Thinking").expect("committed row");
    // The reducer's anchor was minted after this test's clock, so the committed
    // duration is the 4,100 ms advanced minus that setup skew: assert the shape
    // and bounds, not a clock-skew-dependent digit.
    assert!(
        committed[at].starts_with("│ ◇ Thinking · ") && committed[at].ends_with(" · ~120 tokens"),
        "{}",
        committed[at]
    );
    let tab = f.tabs.active_tab();
    let committed_ms = tab
        .reducer
        .open_turn
        .as_ref()
        .unwrap()
        .parts
        .iter()
        .find_map(|part| match part {
            rustain::domain::models::TurnPart::Reasoning { duration_ms, .. } => *duration_ms,
            _ => None,
        })
        .expect("the part recorded its duration");
    assert!((3_000..=4_100).contains(&committed_ms), "{committed_ms}");
    assert_eq!(
        f.h.state.total_content_height, height,
        "the Stage does not jump"
    );
}

#[test]
fn the_attached_live_row_replaces_the_typing_dots() {
    let conversation = {
        let (f, _) = fixture(80, 40);
        let mut c = f.conversation().clone();
        c.turns.clear();
        c.messages.clear();
        c
    };
    let mut streaming = StreamingState {
        is_streaming: true,
        thinking_buffer: "a".repeat(480),
        ..StreamingState::default()
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    streaming.thinking_started_ms = Some(now_ms - 3_100);
    // An empty conversation while streaming is not the empty state.
    let (rows, _) = attached(&conversation, &streaming, 80, 12);
    let live = rows
        .iter()
        .find(|row| row.starts_with("◇ Thinking… · "))
        .unwrap_or_else(|| panic!("no live row: {rows:?}"));
    assert!(live.ends_with(" · ~120 tokens"), "{live}");
    assert!(
        rows.iter().all(|r| r != "···"),
        "the dots are replaced: {rows:?}"
    );
    // Control: without thinking the dots show.
    streaming.thinking_buffer.clear();
    let (rows, _) = attached(&conversation, &streaming, 80, 12);
    assert!(rows.iter().any(|r| r == "···"), "{rows:?}");
}

#[test]
fn the_live_row_after_an_earlier_part_is_spaced_like_the_committed_row() {
    // Review P5: with a prose part before it, the committed reasoning row gets
    // a spacing row; the live row must carry the same one (no jump on commit).
    let turn = Turn::new("fixture-model".into(), 0);
    let mut f = Fixture::new(80, 24, turn);
    f.tabs.active_tab_mut().view_state.mode = rustain::domain::models::AnchorMode::Following;
    f.h.state.set_auto_scroll(true);
    {
        let tab = f.tabs.active_tab_mut();
        let clock = tab.clock.clone();
        for chunk in [
            StreamChunk::Text {
                content: "Earlier answer.".into(),
                parent_tool_use_id: None,
            },
            think_chunk(&"a".repeat(480)),
        ] {
            reduce(&mut tab.reducer, chunk, &*clock);
        }
        update_streaming_mirror(&tab.reducer, &mut tab.streaming);
    }
    f.paint();
    let live = rows(&f);
    let at = row_starting(&live, "│ ◇ Thinking…").expect("live row");
    assert_eq!(live[at - 1], "│", "a spacing row precedes the live row");
    let height = f.h.state.total_content_height;
    {
        let tab = f.tabs.active_tab_mut();
        let clock = tab.clock.clone();
        reduce(
            &mut tab.reducer,
            StreamChunk::Text {
                content: String::new(),
                parent_tool_use_id: None,
            },
            &*clock,
        );
        update_streaming_mirror(&tab.reducer, &mut tab.streaming);
    }
    f.paint();
    let committed = rows(&f);
    let at = row_starting(&committed, "│ ◇ Thinking ·").expect("committed row");
    assert_eq!(committed[at - 1], "│");
    assert_eq!(f.h.state.total_content_height, height, "no jump on commit");
}
