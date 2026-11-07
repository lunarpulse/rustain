//! Smoke data of Story 21.9 (D20): the stored session the native smoke reopens,
//! written by the production serializer from a conversation the production
//! reducer built, and the actual owning-runtime paints the offline desk-check
//! consumes (`RUSTAIN_STAGE_POLISH_FRAMES`). Both generators are `#[ignore]`d.
use std::path::PathBuf;
use std::time::Duration;

use rustain::domain::models::{
    Conversation, MessageRole, StopReason, StreamChunk, TurnPart, UsageInfo, WriteDiffState,
    conversation::PersistedConversation, turn::reasoning_item_id_for,
};
use rustain::domain::services::reducer::{reduce, test_reducer_state};
use serde_json::json;

use super::support::{Fixture, focus, open_focused, screen_rows};
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

fn reasoning_id(conversation: &Conversation) -> String {
    let turn = &conversation.turns[0];
    let part = turn
        .parts
        .iter()
        .find_map(|part| match part {
            TurnPart::Reasoning { id, .. } => Some(*id),
            _ => None,
        })
        .expect("the smoke turn thinks");
    reasoning_item_id_for(&turn.id, part)
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

fn frames_dir() -> Option<PathBuf> {
    std::env::var_os("RUSTAIN_STAGE_POLISH_FRAMES").map(PathBuf::from)
}

fn save(f: &Fixture, dir: &std::path::Path, size: &str, name: &str) {
    let screen = screen_rows(f).join("\n");
    let record = json!({"size": size, "name": name, "screen": screen});
    std::fs::write(
        dir.join(format!("{size}-{name}.json")),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

/// Export the owning-runtime paints at 60x16, 80x24 and 130x30 for the offline
/// desk-check: `stage` (unfocused), `marked` (thinking row focused), `opened`
/// (the S-BLOCK window) and `back` (after Esc).
#[test]
#[ignore = "exports frames to $RUSTAIN_STAGE_POLISH_FRAMES for the smoke desk-check"]
fn story21_9_smoke_frames_export() {
    let dir = frames_dir().expect("RUSTAIN_STAGE_POLISH_FRAMES names the output directory");
    std::fs::create_dir_all(&dir).unwrap();
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let size = format!("{width}x{height}");
        let conversation = smoke_conversation();
        let id = reasoning_id(&conversation);
        let mut f = Fixture::new(width, height, conversation.turns[0].clone());
        f.tabs.active_tab_mut().conversation = conversation;
        f.load();
        f.paint();
        // The unfocused Stage as a fresh session shows it: composer focus (no
        // marker), scrolled to the top so every size shows the user row.
        f.h.state.focus = rustain::domain::models::FocusState::Input;
        let top =
            f.h.state
                .total_content_height
                .saturating_sub(usize::from(f.h.state.viewport_height));
        f.tabs.active_tab_mut().view_state.scroll_offset = top;
        f.h.state.set_scroll_offset(top);
        f.paint();
        save(&f, &dir, &size, "stage");
        f.h.state.focus = rustain::domain::models::FocusState::Chat;
        focus(&mut f, &id);
        f.paint();
        save(&f, &dir, &size, "marked");
        open_focused(&mut f);
        f.compose();
        save(&f, &dir, &size, "opened");
        f.press(rustain::adapters::tui::keymap::Key::Esc);
        f.paint();
        save(&f, &dir, &size, "back");
    }
}

// ── slice 9-B: tool rows and the launchpad ──────────────────────────────────

pub const TOOLS_ID: &str = "story21-9-tool-lines";
pub const TOOLS_TITLE: &str = "Tool lines smoke";
pub const TOOLS_BRANCH: &str = "smoke-21-9";

fn tools_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests_tui/fixtures/sessions/story21_9_tool_lines.json")
}

/// `user`, then one assistant turn the reducer folds from four finished calls
/// (Read with a range, Edit +12 −3, Write of a new 40-line file, Bash) whose
/// durations are the fixture timestamps the smoke asserts.
pub fn tools_conversation() -> Conversation {
    let (mut state, clock) = test_reducer_state(1_700_000_000_000);
    let mut feed = |chunk: StreamChunk| {
        let _ = reduce(&mut state, chunk, &clock);
    };
    let old: String = (1..=3).map(|i| format!("old {i}\n")).collect();
    let new: String = (1..=12).map(|i| format!("new {i}\n")).collect();
    let content: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    let calls = [
        (
            "t1",
            "Read",
            json!({"file_path": "src/x.rs", "offset": 0, "limit": 80}),
            300,
            WriteDiffState::NotAWrite,
        ),
        (
            "t2",
            "Edit",
            json!({"file_path": "src/parser.rs", "old_string": old, "new_string": new}),
            100,
            WriteDiffState::NotAWrite,
        ),
        (
            "t3",
            "Write",
            json!({"file_path": "src/new.rs", "content": content}),
            100,
            WriteDiffState::NewFile,
        ),
        (
            "t4",
            "Bash",
            json!({"command": "cargo test --lib"}),
            2_000,
            WriteDiffState::NotAWrite,
        ),
    ];
    for (id, name, input, ms, diff) in calls {
        feed(StreamChunk::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        });
        feed(StreamChunk::TurnComplete {
            stop_reason: StopReason::ToolUse,
        });
        clock.advance(Duration::from_millis(ms));
        feed(StreamChunk::ToolResult {
            id: id.into(),
            content: "done".into(),
            is_error: false,
            operator_private: false,
            diff,
            images: Vec::new(),
        });
    }
    feed(chunk_text("All four calls finished."));
    feed(StreamChunk::TurnComplete {
        stop_reason: StopReason::EndTurn,
    });
    let turn = state.committed_turn.take().expect("the turn committed");
    let mut user = make_msg("u1", MessageRole::User);
    user.content = "show the tool rows".to_owned();
    let mut conversation = make_conversation(vec![user], vec![]);
    conversation.id = TOOLS_ID.to_owned();
    conversation.title = TOOLS_TITLE.to_owned();
    conversation.turns.push(turn);
    conversation.rebuild_messages_mirror();
    conversation
}

#[test]
#[ignore = "writes tests_tui/fixtures/sessions/story21_9_tool_lines.json"]
fn story21_9_tools_session_fixture_generator() {
    let persisted = PersistedConversation::from_conversation_with_exit(&tools_conversation(), true);
    let json = serde_json::to_string_pretty(&persisted).unwrap();
    std::fs::write(tools_fixture_path(), format!("{json}\n")).unwrap();
}

/// The committed tools fixture loads and its four calls carry the timestamps
/// the smoke rows assert (0.3 s, 0.1 s, 0.1 s, 2.0 s).
#[test]
fn story21_9_committed_tools_fixture_loads_with_its_call_durations() {
    let text = std::fs::read_to_string(tools_fixture_path()).expect("fixture committed");
    let conversation = serde_json::from_str::<PersistedConversation>(&text)
        .unwrap()
        .to_conversation();
    assert_eq!(conversation.id, TOOLS_ID);
    let spans: Vec<_> = conversation.turns[0]
        .parts
        .iter()
        .filter_map(|part| match part {
            TurnPart::ToolInvocation {
                tool,
                started_at,
                ended_at,
                ..
            } => Some((tool.as_str(), ended_at.unwrap() - started_at)),
            _ => None,
        })
        .collect();
    assert_eq!(
        spans,
        [
            ("Read", 300),
            ("Edit", 100),
            ("Write", 100),
            ("Bash", 2_000)
        ]
    );
}

/// Frames of the tool rows and the launchpad for the desk-check.
#[test]
#[ignore = "exports frames to $RUSTAIN_STAGE_POLISH_FRAMES for the smoke desk-check"]
fn story21_9_smoke_frames_export_tools_and_launchpad() {
    use rustain::adapters::tui::launchpad::LaunchpadEnv;
    use rustain::domain::services::session_index::SessionIndex;
    let dir = frames_dir().expect("RUSTAIN_STAGE_POLISH_FRAMES names the output directory");
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join(".git")).unwrap();
    std::fs::write(
        workspace.path().join(".git/HEAD"),
        format!("ref: refs/heads/{TOOLS_BRANCH}\n"),
    )
    .unwrap();
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let size = format!("{width}x{height}");
        // The tool rows: the stored session, composer focus, scrolled to the top.
        let conversation = tools_conversation();
        let mut f = Fixture::new(width, height, conversation.turns[0].clone());
        f.tabs.active_tab_mut().conversation = conversation;
        f.load();
        f.h.state.focus = rustain::domain::models::FocusState::Input;
        // The live smoke unfolds with `zR` (a four-tool turn folds by default).
        let turns = f.tabs.active_tab().conversation.turns.clone();
        f.tabs.active_tab_mut().view_state.expand_all(&turns);
        f.paint();
        let top =
            f.h.state
                .total_content_height
                .saturating_sub(usize::from(f.h.state.viewport_height));
        f.tabs.active_tab_mut().view_state.scroll_offset = top;
        f.h.state.set_scroll_offset(top);
        f.paint();
        save(&f, &dir, &size, "tools");
        // The launchpad: a fresh conversation, the stored one listed under Recent.
        let mut f = Fixture::new(width, height, tools_conversation().turns[0].clone());
        {
            let tab = f.tabs.active_tab_mut();
            tab.conversation.turns.clear();
            tab.conversation.messages.clear();
            tab.conversation.id = "fresh".into();
        }
        f.load();
        f.h.state.focus = rustain::domain::models::FocusState::Input;
        f.h.state.terminal_width = width;
        f.h.state.launchpad_env =
            Some(LaunchpadEnv::new(workspace.path().to_path_buf(), |_| false));
        let index = SessionIndex::build(vec![rustain::domain::models::ConversationSummary {
            id: TOOLS_ID.into(),
            title: TOOLS_TITLE.into(),
            created_at: 1_699_999_000,
            updated_at: 1_699_999_000,
            message_count: 2,
            has_fork_source: false,
        }]);
        f.h.conversation = f.tabs.active_tab().conversation.clone();
        f.h.streaming = f.tabs.active_tab().streaming.clone();
        f.refresh();
        let count = f.tabs.tabs().len();
        rustain::infrastructure::runtime::event_loop::render(
            &mut f.h.terminal,
            &mut f.h.state,
            &f.h.conversation,
            &f.h.streaming,
            "claude-opus",
            "anthropic",
            &rustain::adapters::noop::NoOpSecurity,
            count,
            0,
            Some(&mut f.tabs),
            &index,
            128_000,
            &f.core,
        )
        .unwrap();
        save(&f, &dir, &size, "launchpad");
    }
}
