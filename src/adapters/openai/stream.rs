//! Transforms SSE frames into domain `StreamChunk` values for OpenAI-compatible APIs.

use crate::domain::models::StreamChunk;

use super::types::OpenAiStreamEvent;
use crate::adapters::anthropic::sse::SseFrame;

/// Internal state for accumulating tool call data across multiple SSE events.
struct ToolAccumulator {
    id: String,
    name: String,
    arguments_json: String,
}

/// Converts SSE frames into domain StreamChunk values for OpenAI-compatible APIs.
pub struct OpenAiStreamTransformer {
    /// Active tool accumulators indexed by their tool call index.
    active_tools: std::collections::HashMap<usize, ToolAccumulator>,
    /// Cumulative input tokens (extracted from usage when available).
    #[allow(dead_code)]
    input_tokens: u32,
    /// Cumulative output tokens.
    #[allow(dead_code)]
    output_tokens: u32,
}

impl Default for OpenAiStreamTransformer {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiStreamTransformer {
    pub fn new() -> Self {
        Self {
            active_tools: std::collections::HashMap::new(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    /// Transform an SSE frame into zero or more domain StreamChunks.
    pub fn transform(&mut self, frame: &SseFrame) -> Vec<StreamChunk> {
        // Skip [DONE] sentinel
        if frame.data.trim() == "[DONE]" {
            return vec![];
        }

        // Parse the SSE data as an OpenAI stream event
        let event: OpenAiStreamEvent = match serde_json::from_str(&frame.data) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse OpenAI SSE event data: {} — raw: {}",
                    e,
                    frame.data
                );
                return vec![];
            }
        };

        let mut chunks = Vec::new();

        for choice in &event.choices {
            let delta = &choice.delta;

            // Text content
            if let Some(text) = &delta.content {
                if !text.is_empty() {
                    chunks.push(StreamChunk::Text {
                        content: text.clone(),
                        parent_tool_use_id: None,
                    });
                }
            }

            // Tool call deltas
            if let Some(tool_calls) = &delta.tool_calls {
                for tc in tool_calls {
                    if let Some(id) = &tc.id {
                        // New tool call starting
                        self.active_tools.insert(
                            tc.index,
                            ToolAccumulator {
                                id: id.clone(),
                                name: tc
                                    .function
                                    .as_ref()
                                    .and_then(|f| f.name.clone())
                                    .unwrap_or_default(),
                                arguments_json: String::new(),
                            },
                        );
                    } else if let Some(function) = &tc.function {
                        // Accumulating arguments
                        if let Some(args) = &function.arguments {
                            if let Some(tool) = self.active_tools.get_mut(&tc.index) {
                                tool.arguments_json.push_str(args);
                            }
                        }
                        if let Some(name) = &function.name {
                            if let Some(tool) = self.active_tools.get_mut(&tc.index) {
                                tool.name = name.clone();
                            }
                        }
                    }
                }
            }

            // Finish reason — emit TurnComplete or ToolUse
            if let Some(reason) = &choice.finish_reason {
                match reason.as_str() {
                    "stop" => {
                        chunks.push(StreamChunk::TurnComplete {
                            stop_reason: crate::domain::models::StopReason::EndTurn,
                        });
                    }
                    "tool_calls" => {
                        // Emit all accumulated tool calls
                        for (_idx, tool) in self.active_tools.drain() {
                            let input: serde_json::Value =
                                serde_json::from_str(&tool.arguments_json).unwrap_or_else(|e| {
                                    tracing::warn!("Failed to parse tool arguments JSON: {}", e);
                                    serde_json::Value::Object(serde_json::Map::new())
                                });
                            chunks.push(StreamChunk::ToolUse {
                                id: tool.id,
                                name: tool.name,
                                input,
                            });
                        }
                        chunks.push(StreamChunk::TurnComplete {
                            stop_reason: crate::domain::models::StopReason::ToolUse,
                        });
                    }
                    "length" => {
                        chunks.push(StreamChunk::TurnComplete {
                            stop_reason: crate::domain::models::StopReason::MaxTokens,
                        });
                    }
                    _ => {
                        chunks.push(StreamChunk::TurnComplete {
                            stop_reason: crate::domain::models::StopReason::EndTurn,
                        });
                    }
                }
            }
        }

        chunks
    }
}
