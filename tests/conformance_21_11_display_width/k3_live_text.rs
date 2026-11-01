//! K3 — live transcript text wiring (Story 21.11, AC3 + the transcript half of
//! AC4's highlight rule).
//!
//! Every test drives the real `chat_pane::render_with_search` front door into a
//! `TestBackend` buffer at the supported 60/80/130 widths and reads the painted
//! cells back. Expected rows are built from the C/G/A fixtures, never from the
//! production wrapping formula.

use super::support::*;

use std::collections::{BTreeMap, HashMap};

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane::{self, word_wrap};
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    ChatMessage, ContentBlockType, Conversation, InvocationStatus, LivenessSnapshot, MessageRole,
    StopReason, StreamingState, Turn, ViewState,
};
use rustain::domain::services::search::{SearchMatch, find_matches};
use unicode_width::UnicodeWidthStr;

use crate::render_helpers::{make_conversation, make_msg, make_turn, prose, tool};

const WIDTHS: [u16; 3] = [60, 80, 130];
const PEER_GUTTER: &str = "┆  ";

struct Chat {
    painted: Painted,
    /// `RenderResult::total_content_height` of the same frame.
    total_height: usize,
}

#[allow(clippy::too_many_arguments)]
fn paint_chat(
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    view: &ViewState,
    liveness: Option<&LivenessSnapshot>,
    query: Option<&str>,
    focused: Option<&SearchMatch>,
    matches: &[SearchMatch],
    width: u16,
    height: u16,
) -> Chat {
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    clock.set_frame(3);
    let theme = Theme::dark();
    let mut total_height = 0usize;
    let painted = paint(width, height, |frame| {
        let mut tab = TabRenderState::default();
        let result = chat_pane::render_with_search(
            frame,
            Rect::new(0, 0, width, height),
            conversation,
            open_turn,
            &StreamingState::default(),
            view,
            &clock,
            0,
            true,
            &theme,
            &mut tab,
            &HashMap::new(),
            &BTreeMap::new(),
            query,
            focused,
            matches,
            &[],
            None,
            liveness,
            None, // open_prose
            None, // current_focus
        );
        total_height = result.total_content_height;
    });
    Chat {
        painted,
        total_height,
    }
}

fn simple_chat(conversation: &Conversation, width: u16) -> Chat {
    paint_chat(
        conversation,
        None,
        &ViewState::default(),
        None,
        None,
        None,
        &[],
        width,
        60,
    )
}

fn nonblank_rows(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
    rows(buffer).into_iter().filter(|r| !r.is_empty()).collect()
}

fn peer_message(content: &str) -> ChatMessage {
    let mut message = make_msg("peer-1", MessageRole::User);
    message.content = content.to_owned();
    message.content_blocks = vec![ContentBlockType::PeerText];
    message
}

fn error_message(content: &str) -> ChatMessage {
    let mut message = make_msg("err-1", MessageRole::System);
    message.content = content.to_owned();
    message.content_blocks = vec![ContentBlockType::Error];
    message
}

fn user_message(content: &str) -> ChatMessage {
    let mut message = make_msg("user-1", MessageRole::User);
    message.content = content.to_owned();
    message
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect()
}

/// Render one message at `width`; the painted physical rows must equal the
/// computed height, every content row must fit the content budget, and the
/// source words must be reconstructable from the rows (no loss, no duplicate).
/// Returns the content rows with the gutter removed.
fn assert_message_wraps(
    message: ChatMessage,
    gutter: &str,
    source: &str,
    width: u16,
) -> Vec<String> {
    let conversation = make_conversation(vec![message], vec![]);
    let chat = simple_chat(&conversation, width);
    let painted = nonblank_rows(&chat.painted.buffer);
    assert_eq!(
        painted.len(),
        chat.total_height,
        "width {width}: computed height must equal painted physical rows\n{}",
        painted.join("\n")
    );
    let budget = usize::from(width) - gutter.width();
    let mut bodies = Vec::new();
    for row in &painted[1..] {
        let body = row
            .strip_prefix(gutter)
            .unwrap_or_else(|| panic!("width {width}: row {row:?} lost its gutter {gutter:?}"));
        assert!(
            body.width() <= budget,
            "width {width}: row {row:?} is {} cells, content budget {budget}",
            body.width()
        );
        bodies.push(body.to_owned());
    }
    assert_eq!(
        squash(&bodies.concat()),
        squash(source),
        "width {width}: source words were lost or duplicated"
    );
    assert_whole_clusters(&chat.painted.buffer);
    assert_no_partial_family(&screen(&chat.painted.buffer));
    bodies
}

fn mixed_source() -> String {
    format!(
        "note {} {} {} {} {} tail",
        C.repeat(3),
        G.repeat(45),
        A.repeat(70),
        "x".repeat(150),
        C.repeat(70),
    )
}

#[test]
fn k3_peer_message_height_equals_painted_rows_with_gutter() {
    let source = mixed_source();
    for width in WIDTHS {
        let bodies = assert_message_wraps(peer_message(&source), PEER_GUTTER, &source, width);
        assert!(bodies.len() > 5, "width {width}: long tokens must wrap");
        // Whole-cluster accounting across the whole screen.
        let conversation = make_conversation(vec![peer_message(&source)], vec![]);
        let chat = simple_chat(&conversation, width);
        let text = screen(&chat.painted.buffer);
        assert_eq!(
            count_cluster(&text, G),
            45,
            "width {width}: G lost or split"
        );
    }
}

#[test]
fn k3_error_message_height_equals_painted_rows() {
    let source = mixed_source();
    for width in WIDTHS {
        let bodies = assert_message_wraps(error_message(&source), "", &source, width);
        assert!(bodies.len() > 5, "width {width}: long tokens must wrap");
        let conversation = make_conversation(vec![error_message(&source)], vec![]);
        let chat = simple_chat(&conversation, width);
        // Error style is kept on the first painted content glyph.
        let theme = Theme::dark();
        let cell = chat
            .painted
            .buffer
            .cell(ratatui::layout::Position::new(0, 1))
            .expect("first content cell");
        assert_eq!(cell.fg, theme.colors.error, "width {width}");
        assert_eq!(count_cluster(&screen(&chat.painted.buffer), G), 45);
    }
}

#[test]
fn k3_peer_and_error_tokens_fit_exactly_and_overflow_by_one_unit() {
    for width in WIDTHS {
        type Builder = fn(&str) -> ChatMessage;
        let cases: [(&str, Builder, usize); 2] = [
            (PEER_GUTTER, peer_message, usize::from(width) - 3),
            ("", error_message, usize::from(width)),
        ];
        for (gutter, build, budget) in cases {
            for unit in ["x", A, C, G] {
                let unit_cells = unit.width();
                let per_row = budget / unit_cells;
                let fit = unit.repeat(per_row);
                assert!(fit.width() <= budget && fit.width() + unit_cells > budget);

                let one_row = assert_message_wraps(build(&fit), gutter, &fit, width);
                assert_eq!(one_row, vec![fit.clone()], "exact fit, width {width}");

                let over = unit.repeat(per_row + 1);
                let two_rows = assert_message_wraps(build(&over), gutter, &over, width);
                assert_eq!(
                    two_rows,
                    vec![fit.clone(), unit.to_owned()],
                    "one unit over, width {width}, unit {unit:?}"
                );

                // Fit minus one unit still occupies a single row.
                let minus_one = unit.repeat(per_row - 1);
                let rows = assert_message_wraps(build(&minus_one), gutter, &minus_one, width);
                assert_eq!(
                    rows,
                    vec![minus_one.clone()],
                    "fit minus one unit, width {width}, unit {unit:?}"
                );
            }
        }
    }
}

#[test]
fn k3_peer_words_join_by_cells_before_wrapping() {
    // Two 20-cell words and a separator are 41 cells: they share a row only
    // when the content budget is at least 41, so the 57-cell budget at width
    // 60 keeps them together and a 40-cell budget would not.
    let source = format!("{} {}", C.repeat(10), C.repeat(10));
    let rows = assert_message_wraps(peer_message(&source), PEER_GUTTER, &source, 60);
    assert_eq!(rows, vec![source.clone()]);
    let split = word_wrap::wrap_text(&source, 40);
    assert_eq!(split, vec![C.repeat(10), C.repeat(10)]);
}

#[test]
fn k3_shared_wrapper_wraps_cjk_at_content_width_40_instead_of_clipping() {
    // The attached consent card calls this wrapper with a 40-cell content
    // width; C x 30 is 60 cells and must wrap, not take a scalar-count fast
    // path (30 scalars < 40).
    let rows = word_wrap::wrap_text(&C.repeat(30), 40);
    assert_eq!(rows, vec![C.repeat(20), C.repeat(10)]);
    let rows = word_wrap::wrap_text(&format!("{}{}", G.repeat(30), "xx"), 40);
    assert_eq!(rows, vec![G.repeat(20), format!("{}xx", G.repeat(10))]);
}

fn code_rows(content: &str, width: u16) -> Vec<String> {
    let conversation = make_conversation(vec![user_message(content)], vec![]);
    let chat = simple_chat(&conversation, width);
    assert_whole_clusters(&chat.painted.buffer);
    assert_no_partial_family(&screen(&chat.painted.buffer));
    nonblank_rows(&chat.painted.buffer)
}

#[test]
fn k3_markdown_code_clip_keeps_whole_clusters_at_real_widths() {
    for width in WIDTHS {
        let inner = usize::from(width) - 2;
        let fence = |line: &str| format!("```\n{line}\n```");

        // ASCII positive control: exact fit and one over.
        let fit = code_rows(&fence(&"a".repeat(inner)), width);
        assert_eq!(fit[2], format!("│{}│", "a".repeat(inner)));
        let over = code_rows(&fence(&"a".repeat(inner + 1)), width);
        assert_eq!(over[2], format!("│{}→│", "a".repeat(inner - 1)));

        // A (one cell, two scalars): exact fit and one over.
        let fit = code_rows(&fence(&A.repeat(inner)), width);
        assert_eq!(fit[2], format!("│{}│", A.repeat(inner)));
        let over = code_rows(&fence(&A.repeat(inner + 1)), width);
        assert_eq!(over[2], format!("│{}→│", A.repeat(inner - 1)));

        // G and C (two cells): exact fit, then clipped with a padded gap where
        // the next glyph would have been cut.
        for unit in [G, C] {
            let fit = code_rows(&fence(&unit.repeat(inner / 2)), width);
            assert_eq!(
                fit[2],
                format!("│{}│", unit.repeat(inner / 2)),
                "{unit:?} fit"
            );
            let over = code_rows(&fence(&unit.repeat(inner / 2 + 3)), width);
            let whole = (inner - 1) / 2;
            let gap = " ".repeat(inner - 1 - whole * 2);
            assert_eq!(
                over[2],
                format!("│{}{gap}→│", unit.repeat(whole)),
                "{unit:?} clipped at width {width}"
            );
            assert_eq!(over[2].width(), usize::from(width));
        }
    }
}

#[test]
fn k3_markdown_prose_oversized_token_keeps_whole_clusters_at_real_widths() {
    for width in WIDTHS {
        let cells = usize::from(width);
        // ASCII control.
        let rows = code_rows(&"a".repeat(cells + 5), width);
        assert_eq!(rows[1], "a".repeat(cells));
        // A is one cell; G and C fill an even width exactly.
        let rows = code_rows(&A.repeat(cells + 5), width);
        assert_eq!(rows[1], A.repeat(cells));
        for unit in [G, C] {
            let rows = code_rows(&unit.repeat(cells / 2 + 5), width);
            assert_eq!(rows[1], unit.repeat(cells / 2), "{unit:?} width {width}");
        }
    }
}

fn collapsed_row(sentence: &str, width: u16) -> String {
    let msg = make_msg("t1", MessageRole::Assistant);
    let turn = make_turn(
        "t1",
        vec![
            prose(sentence),
            tool("Read", InvocationStatus::Success),
            tool("Grep", InvocationStatus::Success),
            tool("Bash", InvocationStatus::Success),
        ],
        Some(StopReason::EndTurn),
    );
    let conversation = make_conversation(vec![msg], vec![turn.clone()]);
    let mut view = ViewState::default();
    view.collapsed.insert(turn.id.clone(), true);
    let chat = paint_chat(&conversation, None, &view, None, None, None, &[], width, 20);
    assert_whole_clusters(&chat.painted.buffer);
    assert_no_partial_family(&screen(&chat.painted.buffer));
    nonblank_rows(&chat.painted.buffer)
        .into_iter()
        .next()
        .expect("collapsed summary row")
}

#[test]
fn k3_collapsed_turn_summary_cuts_on_a_whole_grapheme_inside_the_budget() {
    for width in WIDTHS {
        // The ASCII row measures the real prose budget after the glyph, tier
        // and gutter reservation: it holds budget-1 letters plus the marker.
        let control = collapsed_row(&"q".repeat(200), width);
        let letters = control.matches('q').count();
        assert!(letters > 10, "width {width}: control row {control:?}");

        let family = collapsed_row(&G.repeat(100), width);
        assert_eq!(
            count_cluster(&family, G),
            letters / 2,
            "width {width}: whole families within the same budget\n{family}"
        );
        let wide = collapsed_row(&C.repeat(100), width);
        assert_eq!(
            wide.matches(C).count(),
            letters / 2,
            "width {width}\n{wide}"
        );
        let accent = collapsed_row(&A.repeat(200), width);
        assert_eq!(
            count_cluster(&accent, A),
            letters,
            "width {width}\n{accent}"
        );
    }
}

#[test]
fn k3_live_tail_line_cuts_on_a_whole_grapheme_inside_the_budget() {
    for width in WIDTHS {
        let tail_rows = |tail: &str, marker: &str| -> String {
            let turn = make_turn(
                "lr",
                vec![
                    prose("Let me run."),
                    tool("Bash", InvocationStatus::Running),
                ],
                None,
            );
            let conversation = make_conversation(
                vec![make_msg("lr", MessageRole::Assistant)],
                vec![turn.clone()],
            );
            let liveness = LivenessSnapshot {
                active_tool_name: Some("Bash".to_owned()),
                progress: None,
                tail: Some(tail.to_owned()),
            };
            let chat = paint_chat(
                &conversation,
                Some(&turn),
                &ViewState::default(),
                Some(&liveness),
                None,
                None,
                &[],
                width,
                30,
            );
            assert_whole_clusters(&chat.painted.buffer);
            assert_no_partial_family(&screen(&chat.painted.buffer));
            nonblank_rows(&chat.painted.buffer)
                .into_iter()
                .find(|row| row.contains(marker))
                .unwrap_or_else(|| panic!("no tail row containing {marker:?}"))
        };
        let control = tail_rows(&"q".repeat(200), "q");
        let letters = control.matches('q').count();
        assert!(letters > 10, "width {width}: control {control:?}");
        assert!(control.ends_with('…'), "{control:?}");

        let family = tail_rows(&G.repeat(100), "👨");
        assert_eq!(
            count_cluster(&family, G),
            letters / 2,
            "width {width}\n{family}"
        );
        assert!(family.ends_with('…'), "{family:?}");
        let accent = tail_rows(&A.repeat(200), "e\u{301}");
        assert_eq!(
            count_cluster(&accent, A),
            letters,
            "width {width}\n{accent}"
        );
    }
}

/// Glyph cells of `symbol` anywhere in the buffer: `(x, y, modifier)`.
fn glyph_cells(buffer: &ratatui::buffer::Buffer, symbol: &str) -> Vec<(u16, u16, Modifier)> {
    let area = buffer.area;
    let mut found = Vec::new();
    for y in area.top()..area.bottom() {
        let mut x = area.left();
        while x < area.right() {
            let Some(cell) = buffer.cell(ratatui::layout::Position::new(x, y)) else {
                break;
            };
            let sym = cell.symbol();
            if sym == symbol {
                found.push((x, y, cell.modifier));
            }
            x = x.saturating_add(u16::try_from(sym.width().max(1)).unwrap_or(1));
        }
    }
    found
}

fn paint_search(
    content: &str,
    query: &str,
    focused: usize,
    width: u16,
) -> (Chat, Vec<SearchMatch>) {
    let conversation = make_conversation(vec![user_message(content)], vec![]);
    let matches = find_matches(&conversation, query);
    let chat = paint_chat(
        &conversation,
        None,
        &ViewState::default(),
        None,
        Some(query),
        matches.get(focused),
        &matches,
        width,
        20,
    );
    (chat, matches)
}

#[test]
fn k3_transcript_highlight_paints_the_whole_family_for_a_partial_query() {
    for width in WIDTHS {
        let content = format!("a {G} b {G} c");
        // ASCII positive control: the plain query paints exactly its cell.
        let (chat, matches) = paint_search(&content, "b", 0, width);
        assert_eq!(matches.len(), 1);
        let cells = glyph_cells(&chat.painted.buffer, "b");
        assert_eq!(cells.len(), 1);
        assert!(cells[0].2.contains(Modifier::REVERSED | Modifier::BOLD));

        // Query 👨 sits inside both G: two logical matches, focus on the second.
        let (chat, matches) = paint_search(&content, "👨", 1, width);
        assert_eq!(matches.len(), 2, "logical match count is unchanged");
        let rows = nonblank_rows(&chat.painted.buffer);
        assert_eq!(rows[1], content, "painting must not change source text");
        let families = glyph_cells(&chat.painted.buffer, G);
        assert_eq!(families.len(), 2, "whole original grapheme painted twice");
        assert!(families[0].2.contains(Modifier::REVERSED));
        assert!(
            !families[0].2.contains(Modifier::BOLD),
            "first is not focused"
        );
        assert!(families[1].2.contains(Modifier::REVERSED | Modifier::BOLD));
        assert!(
            glyph_cells(&chat.painted.buffer, "👨").is_empty(),
            "no bare first-scalar cell may be painted"
        );
        let plain_c = glyph_cells(&chat.painted.buffer, "c");
        assert_eq!(
            plain_c.len(),
            1,
            "the trailing context glyph is painted once"
        );
        assert!(
            !plain_c[0].2.contains(Modifier::REVERSED),
            "text outside the match must not be highlighted"
        );
        assert_whole_clusters(&chat.painted.buffer);
        assert_no_partial_family(&screen(&chat.painted.buffer));
    }
}

#[test]
fn k3_transcript_highlight_merges_overlapping_painted_ranges() {
    for width in WIDTHS {
        // Each G holds two ZWJ joiners: four logical matches, two painted
        // ranges. Focus the second joiner of the first G.
        let content = format!("a {G} b {G} c");
        let (chat, matches) = paint_search(&content, "\u{200d}", 1, width);
        assert_eq!(matches.len(), 4, "logical match count is unchanged");
        let rows = nonblank_rows(&chat.painted.buffer);
        assert_eq!(
            rows[1], content,
            "overlapping painted ranges must not duplicate or drop a suffix"
        );
        let families = glyph_cells(&chat.painted.buffer, G);
        assert_eq!(families.len(), 2);
        assert!(
            families[0].2.contains(Modifier::REVERSED | Modifier::BOLD),
            "the merged range keeps the focused style"
        );
        assert!(families[1].2.contains(Modifier::REVERSED));
        assert!(!families[1].2.contains(Modifier::BOLD));
    }
}

#[test]
fn k3_transcript_highlight_paints_the_base_and_accent_together() {
    for width in WIDTHS {
        let content = format!("q {A} q");
        let (chat, matches) = paint_search(&content, "e", 0, width);
        assert_eq!(matches.len(), 1);
        let rows = nonblank_rows(&chat.painted.buffer);
        assert_eq!(rows[1], content);
        let accents = glyph_cells(&chat.painted.buffer, A);
        assert_eq!(accents.len(), 1, "base letter and accent paint as one cell");
        assert!(accents[0].2.contains(Modifier::REVERSED | Modifier::BOLD));
        assert!(glyph_cells(&chat.painted.buffer, "e").is_empty());
        assert_whole_clusters(&chat.painted.buffer);
    }
}
