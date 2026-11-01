//! K12 — `/export` writes valid CommonMark (Story 21.12, AC11; baseline B17).
//!
//! The front door is `apply_export_command` (the `/export` handler): it writes
//! the document to disk and the test parses the file back with pulldown-cmark.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{
    ChatMessage, Conversation, FocusState, MessageRole, SessionMeta, ToolCallInfo,
};
use rustain::infrastructure::runtime::event_loop::apply_export_command;

use crate::render_helpers::{make_conversation, make_msg};

const EVERY_ELEMENT: &str = "# Title\n\n### Lists\n\n- a\n  - b\n    - c\n\n3. three\n4. four\n\n- [x] done\n\n> quote\n> > nested\n\n| h1 | h2 |\n|----|----|\n| 1 | 2 |\n\n---\n\n![alt](u.png) [link](https://e.test/p) `code` ~~s~~ **b** *i*\n\n```rust\nfn main() {}\n```\n\n<!-- closed -->\n\n    indented code";
const UNCLOSED_FENCE: &str = "Here it is:\n\n```rust\nlet x = 1;";
const UNCLOSED_COMMENT: &str = "Note:\n\n<!-- to be continued";
// One fence line only: a three-backtick export fence would be closed by it and
// the export's own closing fence would then open a block that swallows the rest.
const TOOL_RESULT: &str = "text\n```\nlater";

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH
}

fn message(id: &str, role: MessageRole, content: &str) -> ChatMessage {
    let mut message = make_msg(id, role);
    message.content = content.to_owned();
    message
}

fn conversation() -> Conversation {
    let mut tool_message = message("m2", MessageRole::Assistant, "Ran it.");
    tool_message.tool_calls = vec![
        serde_json::from_value::<ToolCallInfo>(serde_json::json!({
            "id": "t1",
            "name": "bash",
            "input": {"cmd": "echo ```"},
            "result": {"content": TOOL_RESULT, "isError": false},
        }))
        .unwrap(),
    ];
    let mut conversation = make_conversation(
        vec![
            message("m1", MessageRole::User, EVERY_ELEMENT),
            tool_message,
            message("m3", MessageRole::Assistant, UNCLOSED_FENCE),
            message("m4", MessageRole::User, "ok"),
            message("m5", MessageRole::Assistant, UNCLOSED_COMMENT),
            message("m6", MessageRole::System, "final"),
        ],
        vec![],
    );
    conversation.title = "Export".to_owned();
    conversation
}

fn meta() -> SessionMeta {
    SessionMeta {
        version: 1,
        title: "Export".to_owned(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_060,
        message_count: 6,
        bookmarks: vec![],
        fork_source: None,
        imported_from: None,
        extra: serde_json::Map::new(),
        plan_slug: None,
    }
}

async fn exported() -> String {
    let workspace = tempfile::TempDir::new().unwrap();
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    apply_export_command(
        Some("k12.md"),
        &conversation(),
        &meta(),
        workspace.path(),
        &mut state,
    )
    .await;
    std::fs::read_to_string(workspace.path().join(".rustain/exports/k12.md")).unwrap()
}

/// `(heading text, start offset, end offset)` of every `##` heading.
fn message_headings(document: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for (event, range) in Parser::new_ext(document, options()).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H2,
                ..
            }) => open = Some((range.start, String::new())),
            Event::Text(text) => {
                if let Some((_, buffer)) = &mut open {
                    buffer.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(HeadingLevel::H2)) => {
                if let Some((start, text)) = open.take() {
                    out.push((text, start, range.end));
                }
            }
            _ => {}
        }
    }
    out
}

fn events(source: &str) -> Vec<String> {
    Parser::new_ext(source, options())
        .map(|event| format!("{event:?}"))
        .collect()
}

#[tokio::test]
async fn k12_every_message_keeps_its_own_heading_and_its_own_body() {
    let document = exported().await;
    let headings = message_headings(&document);
    let titles: Vec<&str> = headings.iter().map(|(t, _, _)| t.as_str()).collect();
    assert_eq!(headings.len(), 6, "one heading per message, got {titles:?}");
    for (title, expected) in titles.iter().zip([
        "User",
        "Assistant",
        "Assistant",
        "User",
        "Assistant",
        "System",
    ]) {
        assert!(title.starts_with(&format!("{expected} (")), "{titles:?}");
    }

    let closed_fence = format!("{UNCLOSED_FENCE}\n```");
    let closed_comment = format!("{UNCLOSED_COMMENT}\n-->");
    let expected = [
        Some(EVERY_ELEMENT),
        None,
        Some(closed_fence.as_str()),
        Some("ok"),
        Some(closed_comment.as_str()),
        Some("final"),
    ];
    for (index, (_, _, body_start)) in headings.iter().enumerate() {
        let Some(content) = expected[index] else {
            continue;
        };
        let end = headings.get(index + 1).map_or(document.len(), |h| h.1);
        let section = &document[*body_start..end];
        let body = section
            .strip_suffix("---\n\n")
            .unwrap_or_else(|| panic!("message {index} must end with its rule: {section:?}"));
        assert_eq!(
            events(body),
            events(&format!("{content}\n")),
            "message {index}"
        );
    }
    // A message that already closed its blocks is exported untouched.
    assert!(document.contains(&format!("\n{EVERY_ELEMENT}\n\n---\n")));
}

#[tokio::test]
async fn k12_a_backtick_line_in_a_tool_result_cannot_close_the_export_fence() {
    let document = exported().await;
    assert!(
        document.contains("**Result:**\n````\ntext\n```\nlater\n````\n"),
        "the result fence must be longer than the longest backtick run: {document}"
    );
    // The tool input is fenced the same way: its own ``` stays inside.
    assert!(document.contains("````json\n"), "{document}");
    let after_tool = document.find("</details>").expect("tool block closes");
    let headings = message_headings(&document);
    assert!(
        headings.iter().any(|(_, start, _)| *start > after_tool),
        "headings after the tool block must still parse as headings"
    );
}
