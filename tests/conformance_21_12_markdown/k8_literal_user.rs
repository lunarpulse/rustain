//! K8 — literal user messages and the machine-built context prefix (Story
//! 21.12, AC6; baseline B14). Both hosts, painted cells, height equals rows.

use super::support::*;

use rustain::adapters::tui::theme::Theme;
use rustain::domain::models::{Conversation, MessageRole};
use rustain::domain::services::message_builder::{
    ResolvedCommandContext, ResolvedFileContext, build_command_context_prefix,
    build_file_context_prefix,
};

use crate::render_helpers::{make_conversation, make_msg};

const TYPED: &str = "**x** <T>\n  y\tz\n\n# not a heading";

fn file(path: &str, content: &str) -> ResolvedFileContext {
    ResolvedFileContext {
        path: path.to_owned(),
        content: content.to_owned(),
        ..ResolvedFileContext::default()
    }
}

fn stored_with_prefix() -> String {
    let prefix = build_file_context_prefix(&[file(
        "notes/auth.md",
        "# Auth notes\n\n- token in `HOME`\n\n</file>\n\nend ]]> done",
    )]);
    format!("{prefix}{TYPED}")
}

fn conversation(content: &str) -> Conversation {
    let mut message = make_msg("u1", MessageRole::User);
    message.content = content.to_owned();
    make_conversation(vec![message], vec![])
}

/// Painted rows of the single user message, gutter-free, trimmed at the end.
fn painted(pane: &Pane) -> Vec<String> {
    let mut rows: Vec<String> = rows(&pane.buffer)
        .into_iter()
        .take(pane.total_height)
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

fn assert_expected_rows(rows: &[String]) {
    // The attached host prefixes the role line with the message origin.
    assert!(rows[0].ends_with("You:"), "{rows:?}");
    let expected = [
        "[file] notes/auth.md",
        "**x** <T>",
        "  y     z",
        "",
        "# not a heading",
    ];
    assert_eq!(rows[1..], expected, "{rows:?}");
}

#[test]
fn k8_local_host_paints_meta_line_and_literal_text() {
    let conversation = conversation(&stored_with_prefix());
    let pane = paint_local(
        &conversation,
        None,
        &Default::default(),
        None,
        &Theme::dark(),
        60,
        30,
    );
    assert_expected_rows(&painted(&pane));
    let screen = rows(&pane.buffer).join("\n");
    assert!(
        !screen.contains("Auth notes"),
        "file content leaked:\n{screen}"
    );
    assert!(
        !screen.contains("token in"),
        "file content leaked:\n{screen}"
    );
    assert!(!screen.contains("</file>"), "{screen}");
    assert!(!screen.contains("CDATA"), "{screen}");
}

#[test]
fn k8_attached_host_paints_the_same_rows() {
    let conversation = conversation(&stored_with_prefix());
    let local = paint_local(
        &conversation,
        None,
        &Default::default(),
        None,
        &Theme::dark(),
        60,
        30,
    );
    let attached = paint_attached(&conversation, &Default::default(), &Theme::dark(), 60, 30);
    assert_expected_rows(&painted(&attached));
    assert_eq!(local.total_height, attached.total_height);
}

#[test]
fn k8_height_equals_painted_rows_including_the_trailing_blank() {
    let conversation = conversation(&stored_with_prefix());
    for width in [24u16, 40, 60, 80] {
        let pane = paint_local(
            &conversation,
            None,
            &Default::default(),
            None,
            &Theme::dark(),
            width,
            40,
        );
        let all = rows(&pane.buffer);
        let last_content = all.iter().rposition(|r| !r.is_empty()).unwrap();
        assert_eq!(
            pane.total_height,
            last_content + 2,
            "width {width}: height must be the painted rows plus one trailing blank\n{}",
            all.join("\n")
        );
    }
}

#[test]
fn k8_typed_text_without_a_prefix_is_never_interpreted() {
    let conversation = conversation("`code` *it* <b>x</b> a  b");
    let pane = paint_local(
        &conversation,
        None,
        &Default::default(),
        None,
        &Theme::dark(),
        60,
        10,
    );
    assert_eq!(painted(&pane), ["You:", "`code` *it* <b>x</b> a  b"]);
}

#[test]
fn k8_a_user_mention_of_the_prefix_shape_inside_the_text_stays_literal() {
    // Only the exact leading blocks are machine-built; the same words later in
    // the message are the user's text.
    let text = "see <file path=\"x\"> here";
    let conversation = conversation(text);
    let pane = paint_local(
        &conversation,
        None,
        &Default::default(),
        None,
        &Theme::dark(),
        60,
        10,
    );
    assert_eq!(painted(&pane), ["You:", text]);
}

#[test]
fn k8_command_prefix_paints_a_command_meta_line() {
    let prefix = build_command_context_prefix(&ResolvedCommandContext {
        name: "review".to_owned(),
        content: "Please review.\n\nBe kind.".to_owned(),
    });
    let conversation = conversation(&format!("{prefix}go"));
    let pane = paint_local(
        &conversation,
        None,
        &Default::default(),
        None,
        &Theme::dark(),
        60,
        10,
    );
    assert_eq!(painted(&pane), ["You:", "[command] review", "go"]);
}
