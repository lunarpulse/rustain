//! E2E tests for conversation export (Story 4-4 AC11, AC12).
//!
//! Exercises the markdown serializer and the slugify helper directly,
//! plus registry integration (command palette + slash command registry).
//! The event-loop handler `apply_export_command` — which performs the
//! atomic file write — is covered by manual smoke testing against a
//! running TUI; adding unit tests for it would require full filesystem
//! setup which we avoid in this lib-level E2E layer.

mod common;

use rustain::adapters::command_registry::CommandRegistry;
use rustain::domain::models::conversation::{ChatMessage, Conversation};
use rustain::domain::models::{MessageRole, SessionMeta, ToolCallInfo, ToolResultInfo};
use rustain::domain::services::export::{render_conversation_markdown, slugify};

// ── Helpers ────────────────────────────────────────────────────────────────

fn msg(role: MessageRole, content: &str, created_at: i64) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: "m".to_string(),
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at,
        token_count: None,
        stop_reason: None,
        images: vec![],
    }
}

fn msg_with_tool_call(
    role: MessageRole,
    content: &str,
    tool_name: &str,
    tool_input: serde_json::Value,
    tool_result: &str,
) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: "m".to_string(),
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![ToolCallInfo {
            id: "tool-1".to_string(),
            name: tool_name.to_string(),
            input: tool_input,
            result: Some(ToolResultInfo {
                content: tool_result.to_string(),
                is_error: false,
            }),
            started_at_ms: None,
            completed_at_ms: None,
            status: None,
        }],
        created_at: 1_700_000_000,
        token_count: None,
        stop_reason: None,
        images: vec![],
    }
}

fn conv(title: &str, messages: Vec<ChatMessage>) -> Conversation {
    Conversation {
        id: "conv-ffeeddcc-test".to_string(),
        title: title.to_string(),
        messages,
        turns: Vec::new(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_060,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    }
}

fn meta(message_count: usize) -> SessionMeta {
    SessionMeta {
        version: 1,
        title: "Test".to_string(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_060,
        message_count,
        bookmarks: vec![],
        fork_source: None,
        imported_from: None,
        extra: serde_json::Map::new(),
        plan_slug: None,
    }
}

// ── AC11: Frontmatter shape ────────────────────────────────────────────────

#[test]
fn test_e2e_export_frontmatter_contains_required_fields() {
    let c = conv("My Conversation", vec![]);
    let m = meta(0);
    let out = render_conversation_markdown(&c, &m, 1_700_000_200);
    assert!(out.contains("# My Conversation"));
    assert!(out.contains("**Conversation ID:** conv-ffeeddcc-test"));
    assert!(out.contains("**Created:**"));
    assert!(out.contains("**Updated:**"));
    assert!(out.contains("**Messages:** 0"));
    assert!(out.contains("**Exported:**"));
}

#[test]
fn test_e2e_export_empty_title_shows_untitled() {
    let c = conv("", vec![]);
    let m = meta(0);
    let out = render_conversation_markdown(&c, &m, 1_700_000_000);
    assert!(out.starts_with("# (untitled)"));
}

// ── AC11: Message rendering ────────────────────────────────────────────────

#[test]
fn test_e2e_export_renders_user_and_assistant_messages() {
    let c = conv(
        "Chat",
        vec![
            msg(MessageRole::User, "Hello", 1_700_000_000),
            msg(MessageRole::Assistant, "Hi there", 1_700_000_010),
        ],
    );
    let m = meta(2);
    let out = render_conversation_markdown(&c, &m, 1_700_000_020);
    assert!(out.contains("## User"));
    assert!(out.contains("Hello"));
    assert!(out.contains("## Assistant"));
    assert!(out.contains("Hi there"));
}

#[test]
fn test_e2e_export_preserves_verbatim_content() {
    let raw = "Line 1\nLine 2\n\n**bold** and `code` and [link](http://x)";
    let c = conv("Verbatim", vec![msg(MessageRole::User, raw, 0)]);
    let m = meta(1);
    let out = render_conversation_markdown(&c, &m, 0);
    // Markdown content preserved byte-for-byte (minus leading/trailing shaping).
    assert!(out.contains(raw));
}

#[test]
fn test_e2e_export_renders_tool_call_as_details_block() {
    let c = conv(
        "Tool Chat",
        vec![msg_with_tool_call(
            MessageRole::Assistant,
            "I'll check the file",
            "Read",
            serde_json::json!({"path": "main.rs"}),
            "fn main() {}",
        )],
    );
    let m = meta(1);
    let out = render_conversation_markdown(&c, &m, 0);
    assert!(out.contains("### Tool: Read"));
    assert!(out.contains("<details>"));
    assert!(out.contains("<summary>Tool call</summary>"));
    assert!(out.contains("**Input:**"));
    assert!(out.contains("\"path\""));
    assert!(out.contains("**Result:**"));
    assert!(out.contains("fn main()"));
    assert!(out.contains("</details>"));
}

// ── AC11: Slugify semantics ────────────────────────────────────────────────

#[test]
fn test_e2e_slugify_lowercases_and_replaces_non_alphanumerics() {
    assert_eq!(slugify("Hello World"), "hello-world");
    assert_eq!(slugify("CamelCase Title"), "camelcase-title");
    assert_eq!(slugify("with/slashes"), "with-slashes");
}

#[test]
fn test_e2e_slugify_collapses_runs_and_trims() {
    assert_eq!(slugify("  many   spaces  "), "many-spaces");
    assert_eq!(slugify("---"), "conversation");
}

#[test]
fn test_e2e_slugify_clamps_to_50_chars() {
    let long = "a".repeat(120);
    let slug = slugify(&long);
    assert!(slug.chars().count() <= 50);
}

#[test]
fn test_e2e_slugify_empty_falls_back_to_conversation() {
    assert_eq!(slugify(""), "conversation");
    assert_eq!(slugify("!@#$"), "conversation");
}

#[test]
fn test_e2e_slugify_utf8_produces_ascii() {
    // Non-ASCII chars are stripped (replaced with `-`), leaving the ASCII
    // skeleton. Acceptable v1 behavior — users with Unicode titles are a
    // rare edge case, and the filename must be portable across filesystems.
    let slug = slugify("héllo wörld");
    assert!(slug.is_ascii());
    assert!(!slug.is_empty());
}

// ── AC11: Command registry has /export ─────────────────────────────────────

#[test]
fn test_e2e_export_is_registered_as_builtin_command() {
    let cr = CommandRegistry::new();
    // The registry's `filter("")` returns all commands; we scan for `export`.
    let found = cr.filter("");
    assert!(
        found.iter().any(|cmd| cmd.name == "export"),
        "Expected /export to be a built-in slash command"
    );
}

// ── AC11: Determinism ──────────────────────────────────────────────────────

#[test]
fn test_e2e_export_deterministic_for_same_inputs() {
    let c = conv(
        "Chat",
        vec![msg(MessageRole::User, "deterministic", 1_700_000_000)],
    );
    let m = meta(1);
    let out1 = render_conversation_markdown(&c, &m, 1_700_000_123);
    let out2 = render_conversation_markdown(&c, &m, 1_700_000_123);
    assert_eq!(out1, out2);
}

#[test]
fn test_e2e_export_different_exported_at_produces_different_frontmatter() {
    let c = conv("Chat", vec![]);
    let m = meta(0);
    let out1 = render_conversation_markdown(&c, &m, 1_700_000_100);
    let out2 = render_conversation_markdown(&c, &m, 1_700_000_200);
    assert_ne!(out1, out2);
}

// ── AC11 Image stub format (party-mode Fix 28) ───────────────────────────

#[test]
fn test_e2e_export_image_stub_uses_base64_prefix() {
    use rustain::domain::models::conversation::ImageReference;
    let mut c = conv(
        "With images",
        vec![msg(MessageRole::User, "see img", 1_700_000_000)],
    );
    c.messages[0].images = vec![ImageReference {
        file_name: "abc123def456.png".to_string(),
        media_type: "image/png".to_string(),
        original_size: 1024,
    }];
    let m = meta(1);
    let out = render_conversation_markdown(&c, &m, 1_700_000_100);
    // AC11 spec: image stubs render as `![image](base64:<truncated>...)`.
    // Party-mode Fix 28 replaces the earlier `attached-image.png` stub.
    assert!(
        out.contains("![image](base64:"),
        "Image stub must use the `base64:` prefix — got: {}",
        out
    );
    assert!(
        !out.contains("![image](attached-image.png)"),
        "Old attached-image.png stub format must not appear"
    );
}
