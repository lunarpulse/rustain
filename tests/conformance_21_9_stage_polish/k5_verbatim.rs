//! K5 (AC6, PC-3): the user's own text paints exactly as typed in both hosts,
//! on the committed-turn and the legacy message path, after a `[file]` prefix:
//! no HTML is stripped and no markdown applied. Epic AC3 was delivered by 21.12;
//! this is its both-host regression keystone.
use rustain::adapters::tui::{text_geometry, theme::Theme};
use rustain::domain::models::{Conversation, MessageRole, StopReason, StreamingState};
use rustain::domain::services::message_builder::{ResolvedFileContext, build_file_context_prefix};

use crate::conformance_21_12_markdown::support::{
    Pane, paint_attached, paint_local, rows as pane_rows, turn_conversation, user_message,
};
use crate::render_helpers::{make_conversation, make_msg, make_turn, prose};

/// HTML-looking text, markdown punctuation, an escaped pair, two spaces, a tab.
const TYPED: &str = "<T> Vec<String> <b>b</b> *x* # h \\*y\\*  \ta<br>b";

fn user_row(pane: &Pane) -> String {
    pane_rows(&pane.buffer)
        .into_iter()
        .find(|row| row.contains("Vec<String>"))
        .unwrap_or_else(|| {
            panic!(
                "typed row not painted:\n{}",
                pane_rows(&pane.buffer).join("\n")
            )
        })
}

fn expected() -> String {
    text_geometry::expand_tabs(TYPED, 0).trim_end().to_owned()
}

/// The user message alone (the legacy path: no turn at all).
fn legacy(content: &str) -> Conversation {
    make_conversation(vec![user_message(content)], vec![])
}

/// The user message followed by a committed assistant turn (the turn path).
fn with_turn(content: &str) -> Conversation {
    let turn = make_turn("k", vec![prose("An answer.")], Some(StopReason::EndTurn));
    make_conversation(
        vec![user_message(content), make_msg("k", MessageRole::Assistant)],
        vec![turn],
    )
}

fn paint(conversation: &Conversation, attached: bool) -> Pane {
    if attached {
        paint_attached(
            conversation,
            &StreamingState::default(),
            &Theme::dark(),
            80,
            30,
        )
    } else {
        paint_local(
            conversation,
            None,
            &StreamingState::default(),
            None,
            &Theme::dark(),
            80,
            30,
        )
    }
}

#[test]
fn typed_text_paints_verbatim_in_both_hosts_on_both_paths() {
    for attached in [false, true] {
        for conversation in [legacy(TYPED), with_turn(TYPED)] {
            let pane = paint(&conversation, attached);
            assert_eq!(user_row(&pane), expected(), "attached {attached}");
        }
    }
}

#[test]
fn a_file_context_prefix_leaves_the_typed_text_verbatim() {
    let prefix = build_file_context_prefix(&[ResolvedFileContext {
        path: "notes/auth.md".to_owned(),
        content: "# Auth notes\n\n<b>bold</b> *x*".to_owned(),
        ..ResolvedFileContext::default()
    }]);
    let stored = format!("{prefix}{TYPED}");
    for attached in [false, true] {
        let pane = paint(&with_turn(&stored), attached);
        let rows = pane_rows(&pane.buffer);
        assert!(
            rows.iter().any(|row| row == "[file] notes/auth.md"),
            "attached {attached}: {rows:?}"
        );
        assert_eq!(user_row(&pane), expected(), "attached {attached}");
        assert!(
            rows.iter().all(|row| !row.contains("Auth notes")),
            "the context body is never shown: {rows:?}"
        );
    }
}

#[test]
fn the_same_text_as_assistant_prose_renders_markdown() {
    // Positive control: the pipeline is the one that WOULD change the text.
    for attached in [false, true] {
        let pane = paint(&turn_conversation(TYPED), attached);
        let row = user_row(&pane);
        assert!(!row.contains("*x*"), "emphasis consumed: {row}");
        assert!(row.contains(" x "), "{row}");
        assert!(
            row.contains("*y*") && !row.contains("\\*y\\*"),
            "escape resolved: {row}"
        );
        assert!(
            row.contains("<T>"),
            "HTML stays literal text in prose too (D2): {row}"
        );
    }
}
