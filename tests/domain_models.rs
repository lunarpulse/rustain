//! Domain model unit tests — serde roundtrips, error conversions, and basic properties.

use rustain::domain::errors::{
    DomainError, PermissionError, ProviderError, StorageError, ToolError,
};
use rustain::domain::models::{
    ContentBlockType, MessageRole, PermissionMode, StopReason, StreamChunk, UsageInfo,
};

// ── Serde roundtrip tests ───────────────────────────────────────

// Covers: FR1 (streaming), FR2 (content blocks)
#[test]
fn test_stream_chunk_text_roundtrip() {
    let chunk = StreamChunk::Text {
        content: "hello world".into(),
        parent_tool_use_id: None,
    };
    let json = serde_json::to_string(&chunk).unwrap();
    let deserialized: StreamChunk = serde_json::from_str(&json).unwrap();
    assert_eq!(format!("{:?}", chunk), format!("{:?}", deserialized));
}

// Covers: FR1 (streaming), FR2 (content blocks)
#[test]
fn test_stream_chunk_tool_use_roundtrip() {
    let chunk = StreamChunk::ToolUse {
        id: "tool_123".into(),
        name: "bash".into(),
        input: serde_json::json!({"command": "ls -la"}),
    };
    let json = serde_json::to_string(&chunk).unwrap();
    let deserialized: StreamChunk = serde_json::from_str(&json).unwrap();
    assert_eq!(format!("{:?}", chunk), format!("{:?}", deserialized));
}

// Covers: FR1 (streaming)
#[test]
fn test_stream_chunk_turn_complete_roundtrip() {
    let chunk = StreamChunk::TurnComplete {
        stop_reason: StopReason::EndTurn,
    };
    let json = serde_json::to_string(&chunk).unwrap();
    let deserialized: StreamChunk = serde_json::from_str(&json).unwrap();
    assert_eq!(format!("{:?}", chunk), format!("{:?}", deserialized));
}

// Covers: FR1 (streaming)
#[test]
fn test_stream_chunk_usage_roundtrip() {
    let chunk = StreamChunk::Usage {
        usage: UsageInfo {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: Some(10),
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        },
        session_id: Some("sess_abc".into()),
    };
    let json = serde_json::to_string(&chunk).unwrap();
    let deserialized: StreamChunk = serde_json::from_str(&json).unwrap();
    assert_eq!(format!("{:?}", chunk), format!("{:?}", deserialized));
}

// Covers: FR2 (content blocks)
#[test]
fn test_content_block_type_roundtrip() {
    for variant in [
        ContentBlockType::Text,
        ContentBlockType::ToolCall,
        ContentBlockType::ToolResult,
        ContentBlockType::PermissionPrompt,
        ContentBlockType::Error,
        ContentBlockType::Thinking("test".to_owned()),
    ] {
        let json = serde_json::to_string(&variant).unwrap();
        let deserialized: ContentBlockType = serde_json::from_str(&json).unwrap();
        assert_eq!(variant, deserialized, "Roundtrip failed for {:?}", variant);
    }
}

// Covers: FR1 (streaming)
#[test]
fn test_stop_reason_camel_case_serialization() {
    let json = serde_json::to_string(&StopReason::EndTurn).unwrap();
    assert_eq!(json, "\"endTurn\"");
    let json = serde_json::to_string(&StopReason::ToolUse).unwrap();
    assert_eq!(json, "\"toolUse\"");
    let json = serde_json::to_string(&StopReason::MaxTokens).unwrap();
    assert_eq!(json, "\"maxTokens\"");
}

// Covers: FR2 (content blocks)
#[test]
fn test_message_role_roundtrip() {
    let json = serde_json::to_string(&MessageRole::User).unwrap();
    assert_eq!(json, "\"user\"");
    let deserialized: MessageRole = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, MessageRole::User);
}

// Covers: FR25 (permission modes)
#[test]
fn test_permission_mode_roundtrip() {
    let json = serde_json::to_string(&PermissionMode::Normal).unwrap();
    assert_eq!(json, "\"normal\"");
    let json = serde_json::to_string(&PermissionMode::Yolo).unwrap();
    assert_eq!(json, "\"yolo\"");
}

// ── Error conversion chain tests ────────────────────────────────

// Covers: FR14 (retry/backoff)
#[test]
fn test_provider_error_converts_to_domain_error() {
    let provider_err = ProviderError::ConnectionFailed("timeout".into());
    let domain_err = DomainError::from(provider_err);
    // #[error(transparent)] means DomainError displays the inner error's message
    assert_eq!(domain_err.to_string(), "Connection failed: timeout");
}

// Covers: FR10 (session persistence)
#[test]
fn test_storage_error_converts_to_domain_error() {
    let storage_err = StorageError::NotFound("conv_123".into());
    let domain_err = DomainError::from(storage_err);
    assert_eq!(domain_err.to_string(), "Not found: conv_123");
}

// Covers: FR24 (permission prompt)
#[test]
fn test_permission_error_converts_to_domain_error() {
    let perm_err = PermissionError::Blocked("rm -rf /".into());
    let domain_err = DomainError::from(perm_err);
    assert_eq!(domain_err.to_string(), "Command blocked: rm -rf /");
}

// Covers: FR29 (tool execution)
#[test]
fn test_tool_error_converts_to_domain_error() {
    let tool_err = ToolError::NotFound("nonexistent_tool".into());
    let domain_err = DomainError::from(tool_err);
    assert_eq!(domain_err.to_string(), "Tool not found: nonexistent_tool");
}

// ── generate_conversation_id tests ──────────────────────────────

// Covers: FR10 (session persistence)
#[test]
fn test_generate_conversation_id_is_nonempty() {
    let id = rustain::domain::models::generate_conversation_id();
    assert!(!id.is_empty());
}

// Covers: FR10 (session persistence)
#[test]
fn test_generate_conversation_id_is_unique() {
    let id1 = rustain::domain::models::generate_conversation_id();
    let id2 = rustain::domain::models::generate_conversation_id();
    assert_ne!(id1, id2);
}

// ── Default impl tests ─────────────────────────────────────────

// Covers: FR1 (streaming)
#[test]
fn test_streaming_state_default() {
    let state = rustain::domain::models::StreamingState::default();
    assert_eq!(state.phase, rustain::domain::models::StreamingPhase::Idle);
    assert!(state.current_text_buffer.is_empty());
    assert!(state.current_blocks.is_empty());
    assert!(state.active_tool_calls.is_empty());
    assert!(!state.is_streaming);
}

// Covers: FR10 (session persistence)
#[test]
fn test_session_state_default() {
    let state = rustain::domain::models::SessionState::default();
    assert_eq!(state, rustain::domain::models::SessionState::Empty);
}
