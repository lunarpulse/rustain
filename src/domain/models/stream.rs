#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::content::ContentBlockType;
use super::conversation::{ChatMessage, Conversation};
use super::message::MessageRole;
use super::usage::UsageInfo;
use crate::domain::events::ChunkAction;

/// Reason the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Cancelled,
}

/// A single chunk in a streaming response from a provider.
/// The adapter converts wire-format events into these domain chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamChunk {
    Text {
        content: String,
        parent_tool_use_id: Option<String>,
    },
    Thinking {
        content: String,
        parent_tool_use_id: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    Error {
        content: String,
    },
    Blocked {
        content: String,
    },
    TurnComplete {
        stop_reason: StopReason,
    },
    Usage {
        usage: UsageInfo,
        session_id: Option<String>,
    },
    // v0.5+:
    // CompactBoundary,
    // SdkUserUuid { uuid: String },
    // SdkUserSent { uuid: String },
    // SdkAssistantUuid { uuid: String },
    // ContextWindowUpdate { context_window: u32 },
}

/// State machine tracking where we are in the streaming process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamingPhase {
    Idle,
    AccumulatingText,
    InToolCall { tool_id: String },
    InThinking,
    AwaitingToolExecution,
}

/// Transient streaming state (not persisted).
#[derive(Debug, Clone)]
pub struct StreamingState {
    pub phase: StreamingPhase,
    pub current_text_buffer: String,
    pub current_blocks: Vec<super::content::ContentBlockType>,
    pub active_tool_calls: std::collections::HashMap<String, super::tools::ToolCallInfo>,
    pub is_streaming: bool,
}

impl Default for StreamingState {
    fn default() -> Self {
        Self {
            phase: StreamingPhase::Idle,
            current_text_buffer: String::new(),
            current_blocks: Vec::new(),
            active_tool_calls: std::collections::HashMap::new(),
            is_streaming: false,
        }
    }
}

/// Pure domain function: processes a streaming chunk, mutates conversation/streaming state,
/// and returns a `ChunkAction` telling the event loop what to do next.
///
/// No I/O, no async, no tokio. The `now` parameter (unix timestamp seconds) is passed by the
/// caller so this function remains pure.
pub fn apply_chunk(
    conv: &mut Conversation,
    streaming: &mut StreamingState,
    chunk: StreamChunk,
    now: i64,
) -> ChunkAction {
    match chunk {
        StreamChunk::Text { content, .. } => {
            streaming.current_text_buffer.push_str(&content);
            streaming.phase = StreamingPhase::AccumulatingText;
            ChunkAction::NeedsRedraw
        }

        StreamChunk::Thinking { content, .. } => {
            // Thinking text goes to blocks but not to the main text buffer
            streaming.current_blocks.push(ContentBlockType::Thinking);
            streaming.phase = StreamingPhase::InThinking;
            let _ = content; // content used by rendering layer in future stories
            ChunkAction::NeedsRedraw
        }

        StreamChunk::ToolUse { id, name, input } => {
            use super::tools::ToolCallInfo;
            streaming.active_tool_calls.insert(
                id.clone(),
                ToolCallInfo {
                    id: id.clone(),
                    name,
                    input,
                    result: None,
                    started_at_ms: Some(now as u64 * 1000),
                    completed_at_ms: None,
                },
            );
            streaming.phase = StreamingPhase::InToolCall { tool_id: id };
            ChunkAction::NeedsRedraw
        }

        StreamChunk::TurnComplete { stop_reason } => match stop_reason {
            StopReason::ToolUse => {
                streaming.phase = StreamingPhase::AwaitingToolExecution;
                ChunkAction::TurnContinuing
            }
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::Cancelled => {
                let message = ChatMessage {
                    role: MessageRole::Assistant,
                    content: std::mem::take(&mut streaming.current_text_buffer),
                    content_blocks: std::mem::take(&mut streaming.current_blocks),
                    tool_calls: streaming
                        .active_tool_calls
                        .drain()
                        .map(|(_, v)| v)
                        .collect(),
                    created_at: now,
                    token_count: conv.usage.as_ref().map(|u| u.output_tokens),
                };
                conv.messages.push(message);
                streaming.is_streaming = false;
                streaming.phase = StreamingPhase::Idle;
                ChunkAction::TurnComplete {
                    persist: true,
                    trigger_title_generation: conv.messages.len() == 2,
                }
            }
        },

        StreamChunk::Error { content } => {
            tracing::warn!("Stream error chunk: {}", content);
            streaming.current_blocks.push(ContentBlockType::Error);
            ChunkAction::NeedsRedraw
        }

        StreamChunk::Blocked { content } => {
            tracing::warn!("Stream blocked chunk: {}", content);
            streaming.current_blocks.push(ContentBlockType::Error);
            ChunkAction::NeedsRedraw
        }

        StreamChunk::Usage { usage, .. } => {
            conv.usage = Some(usage);
            ChunkAction::None
        }

        StreamChunk::ToolResult { .. } => {
            // ToolResult chunks are not expected during provider streaming;
            // they originate from the tool execution loop (Sprint 1).
            tracing::warn!("Unexpected ToolResult chunk during streaming");
            ChunkAction::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::conversation::generate_conversation_id;

    fn make_conversation() -> Conversation {
        Conversation {
            id: generate_conversation_id(),
            title: String::new(),
            messages: Vec::new(),
            created_at: 1000,
            updated_at: 1000,
            last_response_at: None,
            session_id: None,
            usage: None,
            fork_source: None,
        }
    }

    fn make_streaming() -> StreamingState {
        let mut s = StreamingState::default();
        s.is_streaming = true;
        s
    }

    #[test]
    fn test_apply_chunk_text_accumulates() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Text {
                content: "Hello".into(),
                parent_tool_use_id: None,
            },
            1000,
        );
        assert_eq!(action, ChunkAction::NeedsRedraw);
        assert_eq!(streaming.current_text_buffer, "Hello");
        assert_eq!(streaming.phase, StreamingPhase::AccumulatingText);
    }

    #[test]
    fn test_apply_chunk_text_merges_consecutive() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Text {
                content: "Hello".into(),
                parent_tool_use_id: None,
            },
            1000,
        );
        apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Text {
                content: " world".into(),
                parent_tool_use_id: None,
            },
            1000,
        );
        assert_eq!(streaming.current_text_buffer, "Hello world");
    }

    #[test]
    fn test_apply_chunk_turn_complete_end_turn_finalizes_message() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        // Simulate user message already in conversation
        conv.messages.push(ChatMessage {
            role: MessageRole::User,
            content: "Hi".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 999,
            token_count: None,
        });

        // Accumulate text
        apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Text {
                content: "Hello!".into(),
                parent_tool_use_id: None,
            },
            1000,
        );

        // Complete
        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            1001,
        );

        assert_eq!(
            action,
            ChunkAction::TurnComplete {
                persist: true,
                trigger_title_generation: true, // 2 messages: user + assistant
            }
        );
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[1].role, MessageRole::Assistant);
        assert_eq!(conv.messages[1].content, "Hello!");
        assert_eq!(conv.messages[1].created_at, 1001);
        assert!(!streaming.is_streaming);
        assert_eq!(streaming.phase, StreamingPhase::Idle);
        assert!(streaming.current_text_buffer.is_empty());
    }

    #[test]
    fn test_apply_chunk_turn_complete_tool_use_returns_continuing() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::ToolUse,
            },
            1000,
        );

        assert_eq!(action, ChunkAction::TurnContinuing);
        assert_eq!(streaming.phase, StreamingPhase::AwaitingToolExecution);
        // Message NOT finalized
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn test_apply_chunk_error_pushes_error_block() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Error {
                content: "rate limited".into(),
            },
            1000,
        );

        assert_eq!(action, ChunkAction::NeedsRedraw);
        assert_eq!(streaming.current_blocks, vec![ContentBlockType::Error]);
    }

    #[test]
    fn test_apply_chunk_blocked_pushes_error_block() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Blocked {
                content: "content blocked".into(),
            },
            1000,
        );

        assert_eq!(action, ChunkAction::NeedsRedraw);
        assert_eq!(streaming.current_blocks, vec![ContentBlockType::Error]);
    }

    #[test]
    fn test_apply_chunk_usage_updates_conversation() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Usage {
                usage: UsageInfo {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
                session_id: None,
            },
            1000,
        );

        assert_eq!(action, ChunkAction::None);
        let usage = conv.usage.as_ref().unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn test_apply_chunk_thinking_sets_phase() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Thinking {
                content: "Let me think...".into(),
                parent_tool_use_id: None,
            },
            1000,
        );

        assert_eq!(action, ChunkAction::NeedsRedraw);
        assert_eq!(streaming.phase, StreamingPhase::InThinking);
        assert_eq!(streaming.current_blocks, vec![ContentBlockType::Thinking]);
    }

    #[test]
    fn test_apply_chunk_tool_use_tracks_call() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            1000,
        );

        assert_eq!(action, ChunkAction::NeedsRedraw);
        assert_eq!(
            streaming.phase,
            StreamingPhase::InToolCall {
                tool_id: "tool_1".into()
            }
        );
        let call = streaming.active_tool_calls.get("tool_1").unwrap();
        assert_eq!(call.name, "bash");
        assert_eq!(call.started_at_ms, Some(1_000_000));
    }

    #[test]
    fn test_apply_chunk_unexpected_tool_result_ignored() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::ToolResult {
                id: "tool_1".into(),
                content: "output".into(),
                is_error: false,
            },
            1000,
        );

        assert_eq!(action, ChunkAction::None);
    }

    #[test]
    fn test_apply_chunk_token_count_from_usage() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        // Set usage first
        apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Usage {
                usage: UsageInfo {
                    input_tokens: 10,
                    output_tokens: 25,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
                session_id: None,
            },
            1000,
        );

        // Accumulate and complete
        apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Text {
                content: "Hi".into(),
                parent_tool_use_id: None,
            },
            1000,
        );
        apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            1001,
        );

        assert_eq!(conv.messages[0].token_count, Some(25));
    }

    #[test]
    fn test_apply_chunk_max_tokens_finalizes() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Text {
                content: "partial".into(),
                parent_tool_use_id: None,
            },
            1000,
        );

        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::MaxTokens,
            },
            1001,
        );

        assert_eq!(
            action,
            ChunkAction::TurnComplete {
                persist: true,
                trigger_title_generation: false, // only 1 message
            }
        );
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "partial");
    }

    #[test]
    fn test_apply_chunk_title_generation_only_at_two_messages() {
        let mut conv = make_conversation();
        let mut streaming = make_streaming();

        // No prior messages -> after finalize, len == 1 -> no title gen
        apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Text {
                content: "first".into(),
                parent_tool_use_id: None,
            },
            1000,
        );
        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            1001,
        );
        assert_eq!(
            action,
            ChunkAction::TurnComplete {
                persist: true,
                trigger_title_generation: false,
            }
        );

        // Now add user + assistant (messages.len() will be 3 after) -> no title gen
        conv.messages.push(ChatMessage {
            role: MessageRole::User,
            content: "follow up".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1002,
            token_count: None,
        });
        streaming.is_streaming = true;
        apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::Text {
                content: "second response".into(),
                parent_tool_use_id: None,
            },
            1003,
        );
        let action = apply_chunk(
            &mut conv,
            &mut streaming,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            1004,
        );
        assert_eq!(
            action,
            ChunkAction::TurnComplete {
                persist: true,
                trigger_title_generation: false, // 3 messages, not 2
            }
        );
    }
}
