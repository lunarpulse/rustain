//! K7 — sanitizer behaviour observed at both hosts (Story 21.12, AC4; baseline
//! B10). Streaming closes formatting only in the trailing open paragraph;
//! completed text never gets speculative closers; fences follow CommonMark.

use super::support::*;

use rustain::adapters::tui::theme::Theme;
use rustain::domain::models::{Conversation, StopReason};

use crate::render_helpers::{make_conversation, make_msg, make_turn};

const WIDTH: u16 = 62;
const HEIGHT: u16 = 30;

fn empty_conversation() -> Conversation {
    make_conversation(vec![], vec![])
}

/// Local host open prose: an open turn plus `open_prose`.
fn local_streaming_rows(text: &str) -> (Pane, Vec<String>) {
    let open = make_turn("open", vec![], None);
    let pane = paint_local(
        &empty_conversation(),
        Some(&open),
        &Default::default(),
        Some(text),
        &Theme::dark(),
        WIDTH,
        HEIGHT,
    );
    // The open turn paints a blank row above its prose.
    let rows = content_rows(&pane)
        .into_iter()
        .skip_while(String::is_empty)
        .collect();
    (pane, rows)
}

/// Attached host: `legacy_streaming_lines` behind `render_attached`.
fn attached_streaming_rows(text: &str) -> (Pane, Vec<String>) {
    let pane = paint_attached(
        &empty_conversation(),
        &streaming_buffer(text),
        &Theme::dark(),
        WIDTH,
        HEIGHT,
    );
    let rows: Vec<String> = rows(&pane.buffer)
        .into_iter()
        .skip_while(|r| r.is_empty() || r.starts_with("Assistant:"))
        .collect();
    (pane, rows)
}

fn completed_rows(text: &str) -> (Pane, Vec<String>) {
    let mut message = make_msg("a1", rustain::domain::models::MessageRole::Assistant);
    message.content = text.to_owned();
    message.stop_reason = Some(StopReason::EndTurn);
    let pane = paint_local(
        &make_conversation(vec![message], vec![]),
        None,
        &Default::default(),
        None,
        &Theme::dark(),
        WIDTH,
        HEIGHT,
    );
    let rows: Vec<String> = rows(&pane.buffer)
        .into_iter()
        .skip(1) // role line
        .collect();
    (pane, rows)
}

#[test]
fn k7_streaming_closes_the_open_bold_run_on_both_hosts() {
    let source = "Here is **bold te";
    let (local, local_rows) = local_streaming_rows(source);
    assert_eq!(local_rows[0], "Here is bold te", "{local_rows:?}");
    assert!(is_bold(&local.buffer, "bold te"));
    assert!(!is_bold(&local.buffer, "Here is"));
    assert!(!local_rows.iter().any(|r| r.contains("**")));

    let (attached, attached_rows) = attached_streaming_rows(source);
    assert_eq!(attached_rows[0], "Here is bold te", "{attached_rows:?}");
    assert!(is_bold(&attached.buffer, "bold te"));
}

#[test]
fn k7_completed_text_never_gets_a_speculative_closer() {
    let (pane, rows) = completed_rows("Here is **bold te");
    assert_eq!(rows[0], "Here is **bold te", "{rows:?}");
    assert!(!is_bold(&pane.buffer, "bold te"));
}

#[test]
fn k7_streaming_leaves_literal_asterisks_and_escapes_alone() {
    let (_, rows) = local_streaming_rows("2 * 3");
    assert_eq!(rows[0], "2 * 3");
    let (_, rows) = local_streaming_rows("\\*x");
    assert_eq!(rows[0], "*x");
    let (_, rows) = local_streaming_rows("snake_case_name and 5 * 6");
    assert_eq!(rows[0], "snake_case_name and 5 * 6");
}

#[test]
fn k7_streaming_never_touches_an_earlier_paragraph() {
    let (pane, rows) = local_streaming_rows("first **open\n\nsecond **tail");
    assert_eq!(rows[0], "first **open", "{rows:?}");
    assert_eq!(rows[2], "second tail", "{rows:?}");
    assert!(is_bold(&pane.buffer, "tail"));
    assert!(!is_bold(&pane.buffer, "open"));
}

#[test]
fn k7_streaming_closes_nested_and_code_openers_in_reverse_order() {
    let (pane, rows) = local_streaming_rows("a **b *c `d");
    assert_eq!(rows[0], "a b c d", "{rows:?}");
    assert!(is_bold(&pane.buffer, "b"));
}

#[test]
fn k7_inline_triple_backticks_in_prose_do_not_fabricate_a_code_box() {
    // Baseline B10: an odd count of ``` anywhere appended a closing fence and
    // painted an empty bordered box.
    let (pane, rows) = completed_rows("Type ``` to start a block");
    assert_eq!(rows[0], "Type ``` to start a block", "{rows:?}");
    let screen = rows.join("\n");
    assert!(!screen.contains('┌') && !screen.contains('└'), "{screen}");
    drop(pane);
}

#[test]
fn k7_unclosed_tilde_fence_runs_to_the_end_in_a_closed_box() {
    let (_, rows) = completed_rows("~~~rust\nfn main() {}\nlet x = 1;");
    let screen = rows.join("\n");
    assert!(rows[0].starts_with("┌─ rust "), "{screen}");
    assert!(screen.contains("│fn main() {}"), "{screen}");
    assert!(screen.contains("│let x = 1;"), "{screen}");
    assert!(rows.iter().any(|r| r.starts_with('└')), "{screen}");
}

#[test]
fn k7_unclosed_backtick_fence_streams_as_code_without_closers() {
    let (pane, rows) = local_streaming_rows("```rust\nlet x = **y;");
    let screen = rows.join("\n");
    assert!(screen.contains("│let x = **y;"), "{screen}");
    assert!(!is_bold(&pane.buffer, "y;"));
}

#[test]
fn k7_four_backtick_fence_holds_a_three_backtick_line() {
    let (_, rows) = completed_rows("````md\n```\ninner\n```\n````");
    let screen = rows.join("\n");
    assert!(screen.contains("│```"), "{screen}");
    assert!(screen.contains("│inner"), "{screen}");
    assert_eq!(
        rows.iter().filter(|r| r.starts_with('┌')).count(),
        1,
        "{screen}"
    );
}
