//! Story 11.6 AC-11.6.5 — byte-identical passthrough when the windowing work
//! is NOT selected.
//!
//! The 11.0a default (`StaticPassthroughAssembler`) must still produce messages
//! byte-identical to pre-11.0 `build_api_messages`. We freeze a golden wire
//! payload (`Message` has no `PartialEq`, so we compare `serde_json` values —
//! the 11.0a convention) and diff against it. This guards that adding the
//! windowing assembler / extending `AssembleDiagnostics` did not perturb the
//! default path.
//!
//! Regenerate the golden with `UPDATE_GOLDEN=1 cargo test --test
//! passthrough_unchanged` (the conversation uses fixed ids/timestamps, so the
//! payload is deterministic).

use rustain::domain::models::{
    ChatMessage, ContentBlockType, Conversation, MessageRole, ToolCallInfo, ToolResultInfo,
};
use rustain::domain::ports::ContextAssemblerPort;
use rustain::domain::services::message_builder::build_api_messages;
use rustain::infrastructure::context::StaticPassthroughAssembler;

const GOLDEN_PATH: &str = "tests/fixtures/passthrough_golden.json";

fn user(id: &str, content: &str) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: id.to_string(),
        role: MessageRole::User,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
    }
}

fn assistant(id: &str, content: &str, tool_calls: Vec<ToolCallInfo>) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: id.to_string(),
        role: MessageRole::Assistant,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls,
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
    }
}

fn assistant_thinking(id: &str, content: &str, thinking: &str) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: id.to_string(),
        role: MessageRole::Assistant,
        content: content.to_string(),
        content_blocks: vec![ContentBlockType::Thinking(thinking.to_string())],
        tool_calls: vec![],
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
    }
}

fn tool_call(id: &str, name: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: id.to_string(),
        name: name.to_string(),
        input: serde_json::json!({"file_path": "src/x.rs"}),
        result: Some(ToolResultInfo {
            content: "ok".to_string(),
            is_error: false,
        }),
        started_at_ms: Some(0),
        completed_at_ms: Some(1),
        status: Some("✓ Success".to_string()),
    }
}

/// A fixed conversation exercising the representative `build_api_messages`
/// paths: user→assistant(tools)→user(merged results)→assistant(thinking)→
/// trailing-assistant(tool) flush.
fn fixed_conversation() -> Conversation {
    Conversation {
        id: "conv-golden".into(),
        messages: vec![
            user("u1", "implement auth"),
            assistant("a1", "reading", vec![tool_call("tc-1", "Read")]),
            user("u2", "thanks, now wire it"),
            assistant_thinking("a2", "here is the plan", "deliberating about the design"),
            assistant("a3", "editing", vec![tool_call("tc-2", "Edit")]),
        ],
        turns: vec![],
        ..Default::default()
    }
}

fn wire(conv: &Conversation) -> serde_json::Value {
    let assembled = StaticPassthroughAssembler.assemble(
        conv,
        rustain::domain::models::AssemblyBudget {
            max_tokens: usize::MAX,
        },
    );
    // Sanity: passthrough is byte-identical to the inline builder it replaced.
    let baseline = build_api_messages(conv);
    assert_eq!(
        serde_json::to_value(&assembled.messages).unwrap(),
        serde_json::to_value(&baseline).unwrap(),
        "passthrough .messages must equal build_api_messages output"
    );
    serde_json::to_value(&assembled.messages).expect("messages serialize")
}

#[test]
fn passthrough_matches_frozen_golden() {
    let conv = fixed_conversation();
    let actual = wire(&conv);

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let pretty = serde_json::to_string_pretty(&actual).unwrap();
        std::fs::write(GOLDEN_PATH, pretty).expect("write golden");
        eprintln!("golden updated at {GOLDEN_PATH}");
        return;
    }

    let golden_str = std::fs::read_to_string(GOLDEN_PATH).unwrap_or_else(|_| {
        panic!(
            "missing golden {GOLDEN_PATH}; regenerate with \
             UPDATE_GOLDEN=1 cargo test --test passthrough_unchanged"
        )
    });
    let golden: serde_json::Value =
        serde_json::from_str(&golden_str).expect("golden is valid JSON");

    assert_eq!(
        actual, golden,
        "passthrough wire payload drifted from the frozen golden — the windowing \
         work must not perturb the default StaticPassthroughAssembler path (AC-11.6.5)"
    );
}
