//! Smoke data of Story 21.9 (D20): the stored session the native History smoke reopens
//! (`tests_tui/fixtures/sessions/story21_9_stage_polish.json`, read by the 21_7
//! History leg), written by the production serializer from a conversation the
//! production reducer built, the generator of that fixture (`#[ignore]`d: it
//! writes the file) and the checks that keep the committed fixture honest.
use std::path::PathBuf;
use std::time::Duration;

use rustain::domain::models::{
    Conversation, MessageRole, StopReason, StreamChunk, TurnPart, UsageInfo, WriteDiffState,
    conversation::PersistedConversation,
};
use rustain::domain::services::reducer::{reduce, test_reducer_state};
use serde_json::json;

use crate::render_helpers::{make_conversation, make_msg};

pub const SMOKE_ID: &str = "story21-9-stage-polish";
pub const SMOKE_USER: &str = "explain <T> Vec<String> <b>b</b> *x*";
/// Carries a raw ESC: the window never shows it and `c` must still copy it.
pub const SMOKE_THINK: &str = "Compare parser A and parser B.\n\nB has fewer cases.\u{1b}[0m";
pub const SMOKE_ANSWER: &str = "Parser B wins: it has fewer cases.";

fn chunk_text(content: &str) -> StreamChunk {
    StreamChunk::Text {
        content: content.into(),
        parent_tool_use_id: None,
    }
}

/// `user`, then one assistant turn the reducer folds from the chunks a real
/// response carries: thinking (4.2 s, 1,300 reported tokens), a Read call and
/// the answer.
pub fn smoke_conversation() -> Conversation {
    let (mut state, clock) = test_reducer_state(1_700_000_000_000);
    let mut feed = |chunk: StreamChunk| {
        let _ = reduce(&mut state, chunk, &clock);
    };
    feed(StreamChunk::Thinking {
        content: SMOKE_THINK.into(),
        parent_tool_use_id: None,
    });
    clock.advance(Duration::from_millis(4_200));
    feed(StreamChunk::Usage {
        usage: UsageInfo {
            input_tokens: 120,
            output_tokens: 80,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: Some(1_300),
        },
        session_id: None,
    });
    feed(StreamChunk::ToolUse {
        id: "toolu_smoke".into(),
        name: "Read".into(),
        input: json!({"file_path": "src/x.rs"}),
    });
    feed(StreamChunk::TurnComplete {
        stop_reason: StopReason::ToolUse,
    });
    feed(StreamChunk::ToolResult {
        id: "toolu_smoke".into(),
        content: "fn main() {}".into(),
        is_error: false,
        operator_private: false,
        diff: WriteDiffState::NotAWrite,
        images: Vec::new(),
    });
    feed(chunk_text(SMOKE_ANSWER));
    feed(StreamChunk::TurnComplete {
        stop_reason: StopReason::EndTurn,
    });
    let turn = state.committed_turn.take().expect("the turn committed");
    let mut user = make_msg("u1", MessageRole::User);
    user.content = SMOKE_USER.to_owned();
    let mut conversation = make_conversation(vec![user], vec![]);
    conversation.id = SMOKE_ID.to_owned();
    conversation.title = "Stage polish smoke".to_owned();
    conversation.turns.push(turn);
    conversation.rebuild_messages_mirror();
    conversation
}

#[test]
fn story21_9_smoke_conversation_carries_the_facts_it_is_built_to_show() {
    let conversation = smoke_conversation();
    let facts: Vec<_> = conversation.turns[0]
        .parts
        .iter()
        .filter_map(|part| match part {
            TurnPart::Reasoning {
                text,
                duration_ms,
                tokens,
                ..
            } => Some((text.as_str(), *duration_ms, *tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(facts, [(SMOKE_THINK, Some(4_200), Some(1_300))]);
    assert_eq!(conversation.messages.len(), 2, "user row + assistant row");
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests_tui/fixtures/sessions/story21_9_stage_polish.json")
}

/// Regenerate the committed session fixture with the production serializer:
/// `cargo test --test tui story21_9_smoke_session_fixture -- --ignored`.
#[test]
#[ignore = "writes tests_tui/fixtures/sessions/story21_9_stage_polish.json"]
fn story21_9_smoke_session_fixture_generator() {
    let persisted = PersistedConversation::from_conversation_with_exit(&smoke_conversation(), true);
    let json = serde_json::to_string_pretty(&persisted).unwrap();
    let path = fixture_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("{json}\n")).unwrap();
}

/// The committed fixture loads through the production deserializer with the
/// facts the smoke asserts on (the generator is `#[ignore]`d, so this guards drift).
#[test]
fn story21_9_committed_session_fixture_loads_with_its_reasoning_facts() {
    let text = std::fs::read_to_string(fixture_path()).expect("fixture committed");
    let persisted: PersistedConversation = serde_json::from_str(&text).unwrap();
    let conversation = persisted.to_conversation();
    assert_eq!(conversation.id, SMOKE_ID);
    assert_eq!(conversation.messages[0].content, SMOKE_USER);
    let facts: Vec<_> = conversation.turns[0]
        .parts
        .iter()
        .filter_map(|part| match part {
            TurnPart::Reasoning {
                text,
                duration_ms,
                tokens,
                ..
            } => Some((text.as_str(), *duration_ms, *tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(facts, [(SMOKE_THINK, Some(4_200), Some(1_300))]);
}
