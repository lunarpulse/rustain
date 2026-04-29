use rustain::domain::models::{ChatMessage, Conversation, MessageRole};
use rustain::domain::services::message_builder::{
    ResolvedCommandContext, ResolvedFileContext, build_api_messages, build_command_context_prefix,
    build_file_context_prefix,
};

fn make_conversation(messages: Vec<ChatMessage>) -> Conversation {
    Conversation {
        id: "test".to_string(),
        title: String::new(),
        messages,
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
    }
}

// Covers: FR1 (streaming), FR15 (history rebuild)
#[test]
fn test_build_api_messages_maps_roles() {
    let conv = make_conversation(vec![
        ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::User,
            content: "hello".to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        },
        ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role: MessageRole::Assistant,
            content: "hi there".to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        },
    ]);

    let messages = build_api_messages(&conv);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].content, "hello");
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].content, "hi there");
}

// Covers: FR1 (streaming), FR15 (history rebuild)
#[test]
fn test_build_api_messages_empty_conversation() {
    let conv = make_conversation(vec![]);
    let messages = build_api_messages(&conv);
    assert!(messages.is_empty());
}

// === File context attachment tests (Story 3.2, Task 4) ===

// Covers: AC4 — file context formatting as XML blocks
#[test]
fn test_build_file_context_prefix_single() {
    let files = vec![ResolvedFileContext {
        path: "src/main.rs".to_string(),
        content: "fn main() {}".to_string(),
    }];
    let prefix = build_file_context_prefix(&files);
    assert!(prefix.contains("<file path=\"src/main.rs\">"));
    assert!(prefix.contains("<![CDATA[fn main() {}]]>"));
    assert!(prefix.contains("</file>"));
}

// Covers: AC4 — multiple file mentions produce multiple XML blocks
#[test]
fn test_build_file_context_prefix_multiple() {
    let files = vec![
        ResolvedFileContext {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        },
        ResolvedFileContext {
            path: "src/lib.rs".to_string(),
            content: "pub mod app;".to_string(),
        },
    ];
    let prefix = build_file_context_prefix(&files);
    assert!(prefix.contains("<file path=\"src/main.rs\">"));
    assert!(prefix.contains("<file path=\"src/lib.rs\">"));
    // Both files present inside CDATA
    assert!(prefix.contains("<![CDATA[fn main() {}]]>"));
    assert!(prefix.contains("<![CDATA[pub mod app;]]>"));
}

// Covers: AC4 — empty file list produces empty string
#[test]
fn test_build_file_context_prefix_empty() {
    let prefix = build_file_context_prefix(&[]);
    assert!(prefix.is_empty());
}

// Covers: AC2 — command context formatting as XML block
#[test]
fn test_build_command_context_prefix() {
    let cmd = ResolvedCommandContext {
        name: "deploy-staging".to_string(),
        content: "# Deploy\nRun deploy script.".to_string(),
    };
    let prefix = build_command_context_prefix(&cmd);
    assert!(prefix.contains("<command name=\"deploy-staging\">"));
    assert!(prefix.contains("<![CDATA[# Deploy\nRun deploy script.]]>"));
    assert!(prefix.contains("</command>"));
}

// Covers: P5 — XML special characters in file path are escaped
#[test]
fn test_build_file_context_prefix_escapes_xml_in_path() {
    let files = vec![ResolvedFileContext {
        path: "dir/file<script>.rs".to_string(),
        content: "fn main() {}".to_string(),
    }];
    let prefix = build_file_context_prefix(&files);
    // Path should be escaped — no raw < in attribute
    assert!(prefix.contains("path=\"dir/file&lt;script&gt;.rs\""));
    assert!(!prefix.contains("path=\"dir/file<script>.rs\""));
}

// Covers: P5 — XML special characters in command name are escaped
#[test]
fn test_build_command_context_prefix_escapes_xml() {
    let cmd = ResolvedCommandContext {
        name: "test\"cmd".to_string(),
        content: "content".to_string(),
    };
    let prefix = build_command_context_prefix(&cmd);
    assert!(prefix.contains("name=\"test&quot;cmd\""));
}

// Covers: P3 fix — CDATA end sequence in content is escaped
#[test]
fn test_build_file_context_cdata_escapes_end_sequence() {
    let files = vec![ResolvedFileContext {
        path: "test.rs".to_string(),
        content: "let x = \"]]>\"; // tricky".to_string(),
    }];
    let prefix = build_file_context_prefix(&files);
    // ]]> inside content must be split to avoid breaking CDATA
    assert!(!prefix.contains("<![CDATA[let x = \"]]>\"; // tricky]]>"));
    assert!(prefix.contains("]]]]><![CDATA[>"));
}

// Covers: P15 / Task 4.6 — file context with real Cargo.toml content
#[test]
fn test_file_context_with_real_content() {
    // Simulate resolving @Cargo.toml by reading actual file content
    let cargo_content =
        std::fs::read_to_string("Cargo.toml").expect("Cargo.toml should exist in project root");

    let files = vec![ResolvedFileContext {
        path: "Cargo.toml".to_string(),
        content: cargo_content.clone(),
    }];
    let prefix = build_file_context_prefix(&files);

    // Verify XML structure wraps the file content in CDATA
    assert!(prefix.contains("<file path=\"Cargo.toml\">"));
    assert!(prefix.contains("</file>"));
    assert!(prefix.contains("<![CDATA["));
    // Verify actual Cargo.toml content is present in the payload
    assert!(prefix.contains("[package]"));
    assert!(prefix.contains("rustain"));
}

// Covers: P15 — multiple file mentions produce correct combined payload
#[test]
fn test_file_context_multiple_mentions_payload() {
    let files = vec![
        ResolvedFileContext {
            path: "src/main.rs".to_string(),
            content: "fn main() { println!(\"hello\"); }".to_string(),
        },
        ResolvedFileContext {
            path: "src/lib.rs".to_string(),
            content: "pub mod domain;".to_string(),
        },
    ];
    let prefix = build_file_context_prefix(&files);

    // Both files present as separate XML blocks with CDATA
    assert!(prefix.contains("<file path=\"src/main.rs\">"));
    assert!(prefix.contains("<file path=\"src/lib.rs\">"));
    assert!(prefix.contains("fn main()"));
    assert!(prefix.contains("pub mod domain;"));
    assert!(prefix.contains("<![CDATA["));

    // Combined with user text, simulates full message payload
    let user_text = "explain @src/main.rs vs @src/lib.rs";
    let full_message = format!("{}{}", prefix, user_text);
    assert!(full_message.contains("<file path=\"src/main.rs\">"));
    assert!(full_message.contains("explain @src/main.rs vs @src/lib.rs"));
}
