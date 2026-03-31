//! Transforms SSE frames into domain `StreamChunk` values.
//! Accumulates tool input JSON internally; domain only sees complete tool calls.

use crate::domain::models::{StopReason, StreamChunk, UsageInfo};

use super::sse::SseFrame;
use super::types::{AnthropicEvent, ContentBlockInfo, DeltaType};

/// Internal state for accumulating tool call data across multiple SSE events.
struct ToolAccumulator {
    id: String,
    name: String,
    input_json: String,
}

/// Converts SSE frames into domain StreamChunk values.
///
/// Maintains internal state for tool input JSON accumulation across
/// content_block_delta events. Emits StreamChunk::ToolUse only on
/// content_block_stop, with the complete parsed JSON.
pub struct StreamTransformer {
    /// Active content blocks indexed by their block index.
    /// Tracks tool accumulators for tool_use blocks.
    active_tools: std::collections::HashMap<usize, ToolAccumulator>,
    /// Cumulative input tokens from message_start.
    input_tokens: u32,
    /// Cumulative output tokens from message_delta.
    output_tokens: u32,
    /// Cache tokens from message_start.
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    /// Stop reason extracted from message_delta (used at message_stop).
    pending_stop_reason: Option<StopReason>,
}

impl Default for StreamTransformer {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamTransformer {
    pub fn new() -> Self {
        Self {
            active_tools: std::collections::HashMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            pending_stop_reason: None,
        }
    }

    /// Transform an SSE frame into zero or more domain StreamChunks.
    pub fn transform(&mut self, frame: &SseFrame) -> Vec<StreamChunk> {
        // Parse the SSE data as an Anthropic event
        let event: AnthropicEvent = match serde_json::from_str(&frame.data) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse SSE event data: {} — event: {}, raw: {}",
                    e,
                    frame.event,
                    frame.data
                );
                return vec![];
            }
        };

        match event {
            AnthropicEvent::MessageStart { message } => {
                if let Some(usage) = message.usage {
                    self.input_tokens = usage.input_tokens;
                    self.cache_creation_input_tokens = usage.cache_creation_input_tokens;
                    self.cache_read_input_tokens = usage.cache_read_input_tokens;
                    return vec![StreamChunk::Usage {
                        usage: UsageInfo {
                            input_tokens: self.input_tokens,
                            output_tokens: 0,
                            cache_creation_input_tokens: self.cache_creation_input_tokens,
                            cache_read_input_tokens: self.cache_read_input_tokens,
                        },
                        session_id: None,
                    }];
                }
                vec![]
            }

            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => match content_block {
                ContentBlockInfo::Text { .. } => vec![],
                ContentBlockInfo::ToolUse { id, name } => {
                    self.active_tools.insert(
                        index,
                        ToolAccumulator {
                            id,
                            name,
                            input_json: String::new(),
                        },
                    );
                    vec![]
                }
                ContentBlockInfo::Thinking { .. } => vec![],
            },

            AnthropicEvent::ContentBlockDelta { index, delta } => match delta {
                DeltaType::TextDelta { text } => vec![StreamChunk::Text {
                    content: text,
                    parent_tool_use_id: None,
                }],
                DeltaType::InputJsonDelta { partial_json } => {
                    if let Some(tool) = self.active_tools.get_mut(&index) {
                        tool.input_json.push_str(&partial_json);
                    }
                    vec![]
                }
                DeltaType::ThinkingDelta { thinking } => vec![StreamChunk::Thinking {
                    content: thinking,
                    parent_tool_use_id: None,
                }],
                DeltaType::SignatureDelta { .. } => {
                    // Extended thinking verification — ignored
                    vec![]
                }
            },

            AnthropicEvent::ContentBlockStop { index } => {
                if let Some(tool) = self.active_tools.remove(&index) {
                    let input: serde_json::Value = serde_json::from_str(&tool.input_json)
                        .unwrap_or_else(|e| {
                            tracing::warn!("Failed to parse tool input JSON: {}", e);
                            serde_json::Value::Object(serde_json::Map::new())
                        });
                    vec![StreamChunk::ToolUse {
                        id: tool.id,
                        name: tool.name,
                        input,
                    }]
                } else {
                    vec![]
                }
            }

            AnthropicEvent::MessageDelta { delta, usage } => {
                if let Some(output_usage) = usage {
                    self.output_tokens = output_usage.output_tokens;
                }
                if let Some(reason) = delta.stop_reason.as_deref() {
                    self.pending_stop_reason = Some(match reason {
                        "end_turn" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        other => {
                            tracing::warn!("Unknown stop_reason: {}", other);
                            StopReason::EndTurn
                        }
                    });
                }
                // Emit updated usage
                vec![StreamChunk::Usage {
                    usage: UsageInfo {
                        input_tokens: self.input_tokens,
                        output_tokens: self.output_tokens,
                        cache_creation_input_tokens: self.cache_creation_input_tokens,
                        cache_read_input_tokens: self.cache_read_input_tokens,
                    },
                    session_id: None,
                }]
            }

            AnthropicEvent::MessageStop {} => {
                let stop_reason = self
                    .pending_stop_reason
                    .take()
                    .unwrap_or(StopReason::EndTurn);
                vec![StreamChunk::TurnComplete { stop_reason }]
            }

            AnthropicEvent::Ping {} => vec![],

            AnthropicEvent::Error { error } => vec![StreamChunk::Error {
                content: format!("{}: {}", error.error_type, error.message),
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(event: &str, data: &str) -> SseFrame {
        SseFrame {
            event: event.to_string(),
            data: data.to_string(),
        }
    }

    #[test]
    fn test_transform_text_delta() {
        let mut t = StreamTransformer::new();
        let chunks = t.transform(&frame(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        ));
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::Text { content, .. } => assert_eq!(content, "Hello"),
            _ => panic!("Expected Text chunk"),
        }
    }

    #[test]
    fn test_transform_thinking_delta() {
        let mut t = StreamTransformer::new();
        let chunks = t.transform(&frame(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}"#,
        ));
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::Thinking { content, .. } => assert_eq!(content, "Let me think"),
            _ => panic!("Expected Thinking chunk"),
        }
    }

    #[test]
    fn test_transform_tool_use_accumulated() {
        let mut t = StreamTransformer::new();

        // content_block_start with tool_use
        t.transform(&frame(
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool_1","name":"bash"}}"#,
        ));

        // Accumulate partial JSON
        let c1 = t.transform(&frame(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"com"}}"#,
        ));
        assert!(c1.is_empty()); // No chunk yet

        let c2 = t.transform(&frame(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"mand\":\"ls\"}"}}"#,
        ));
        assert!(c2.is_empty()); // Still accumulating

        // content_block_stop emits the complete ToolUse
        let chunks = t.transform(&frame(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ));
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::ToolUse { id, name, input } => {
                assert_eq!(id, "tool_1");
                assert_eq!(name, "bash");
                assert_eq!(input, &serde_json::json!({"command": "ls"}));
            }
            _ => panic!("Expected ToolUse chunk"),
        }
    }

    #[test]
    fn test_transform_message_start_usage() {
        let mut t = StreamTransformer::new();
        let chunks = t.transform(&frame(
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":100}}}"#,
        ));
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::Usage { usage, .. } => assert_eq!(usage.input_tokens, 100),
            _ => panic!("Expected Usage chunk"),
        }
    }

    #[test]
    fn test_transform_message_delta_and_stop() {
        let mut t = StreamTransformer::new();

        // message_delta with stop_reason
        let c1 = t.transform(&frame(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
        ));
        assert_eq!(c1.len(), 1); // Usage update
        match &c1[0] {
            StreamChunk::Usage { usage, .. } => assert_eq!(usage.output_tokens, 42),
            _ => panic!("Expected Usage chunk"),
        }

        // message_stop emits TurnComplete
        let c2 = t.transform(&frame("message_stop", r#"{"type":"message_stop"}"#));
        assert_eq!(c2.len(), 1);
        match &c2[0] {
            StreamChunk::TurnComplete { stop_reason } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            _ => panic!("Expected TurnComplete"),
        }
    }

    #[test]
    fn test_transform_tool_use_stop_reason() {
        let mut t = StreamTransformer::new();
        t.transform(&frame(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}"#,
        ));
        let chunks = t.transform(&frame("message_stop", r#"{"type":"message_stop"}"#));
        match &chunks[0] {
            StreamChunk::TurnComplete { stop_reason } => {
                assert_eq!(*stop_reason, StopReason::ToolUse);
            }
            _ => panic!("Expected TurnComplete"),
        }
    }

    #[test]
    fn test_transform_error_event() {
        let mut t = StreamTransformer::new();
        let chunks = t.transform(&frame(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"API overloaded"}}"#,
        ));
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::Error { content } => {
                assert!(content.contains("overloaded_error"));
                assert!(content.contains("API overloaded"));
            }
            _ => panic!("Expected Error chunk"),
        }
    }

    #[test]
    fn test_transform_ping_ignored() {
        let mut t = StreamTransformer::new();
        let chunks = t.transform(&frame("ping", r#"{"type":"ping"}"#));
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_transform_unknown_event_ignored() {
        let mut t = StreamTransformer::new();
        let chunks = t.transform(&frame("unknown_event", r#"{"type":"something_new"}"#));
        assert!(chunks.is_empty()); // Logged as warning, no chunks
    }

    #[test]
    fn test_transform_signature_delta_ignored() {
        let mut t = StreamTransformer::new();
        let chunks = t.transform(&frame(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQJ..."}}"#,
        ));
        assert!(chunks.is_empty());
    }
}
