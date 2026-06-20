//! Transforms SSE frames into domain `StreamChunk` values for OpenAI-compatible APIs.

use crate::domain::models::{StreamChunk, UsageInfo};

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
    input_tokens: u32,
    /// Cumulative output tokens.
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

            // DeepSeek v4 reasoning/thinking content
            if let Some(reasoning) = &delta.reasoning_content {
                if !reasoning.is_empty() {
                    chunks.push(StreamChunk::Thinking {
                        content: reasoning.clone(),
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

        // Emit usage on the final SSE chunk (OpenAI sends it when
        // `stream_options.include_usage = true`, with `choices: []`). The turn
        // runtime consumes `StreamChunk::Usage` (last-writer-wins) and appends
        // a `UsageLedgerEntry` — this is the missing link for non-Anthropic
        // token tracking. See case file: openai-usage-calculation.
        if let Some(ref usage) = event.usage {
            self.input_tokens = usage.prompt_tokens;
            self.output_tokens = usage.completion_tokens;
            let reasoning_tokens = usage
                .completion_tokens_details
                .as_ref()
                .and_then(|d| d.reasoning_tokens);
            // OpenAI's `prompt_tokens_details.cached_tokens` = prompt tokens
            // served from the prompt cache. Map to `cache_read_input_tokens`
            // so the cost calculator charges them at the cache-read rate.
            // OpenAI-compat providers never expose Anthropic-style cache
            // *creation*, so that stays None.
            let cache_read_input_tokens = usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens);
            chunks.push(StreamChunk::Usage {
                usage: UsageInfo {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens,
                    reasoning_tokens,
                },
                session_id: None,
            });
        }

        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: &str) -> SseFrame {
        SseFrame {
            event: String::new(),
            data: data.to_string(),
        }
    }

    #[test]
    fn usage_chunk_emits_stream_chunk_usage() {
        // OpenAI sends usage on the final chunk with an empty `choices` array
        // when `stream_options.include_usage = true`.
        let data = r#"{"object":"chat.completion.chunk","choices":[],
            "usage":{"prompt_tokens":42,"completion_tokens":17,"total_tokens":59,
            "completion_tokens_details":{"reasoning_tokens":5}}}"#;
        let mut t = OpenAiStreamTransformer::new();
        let chunks = t.transform(&frame(data));

        // Exactly one chunk: the Usage event.
        assert_eq!(chunks.len(), 1, "expected a single Usage chunk");
        match &chunks[0] {
            StreamChunk::Usage { usage, session_id } => {
                assert_eq!(usage.input_tokens, 42, "prompt_tokens → input_tokens");
                assert_eq!(usage.output_tokens, 17, "completion_tokens → output_tokens");
                assert_eq!(
                    usage.reasoning_tokens,
                    Some(5),
                    "completion_tokens_details.reasoning_tokens propagated"
                );
                assert_eq!(
                    usage.cache_creation_input_tokens, None,
                    "cache fields stay None for OpenAI-compat providers"
                );
                assert_eq!(usage.cache_read_input_tokens, None);
                assert_eq!(*session_id, None, "session_id not set by adapter");
            }
            other => panic!("expected StreamChunk::Usage, got {other:?}"),
        }
    }

    #[test]
    fn usage_without_completion_details_defaults_reasoning_to_none() {
        // Providers that omit `completion_tokens_details` (e.g. vanilla GPT-4o)
        // must deserialize cleanly and yield reasoning_tokens = None.
        let data = r#"{"object":"chat.completion.chunk","choices":[],
            "usage":{"prompt_tokens":100,"completion_tokens":1}}"#;
        let mut t = OpenAiStreamTransformer::new();
        let chunks = t.transform(&frame(data));

        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::Usage { usage, .. } => {
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.output_tokens, 1);
                assert_eq!(usage.reasoning_tokens, None);
                assert_eq!(usage.cache_read_input_tokens, None);
            }
            other => panic!("expected StreamChunk::Usage, got {other:?}"),
        }
    }

    #[test]
    fn cached_prompt_tokens_map_to_cache_read() {
        // OpenAI prompt caching: `prompt_tokens_details.cached_tokens` are
        // prompt tokens served from cache → map to cache_read_input_tokens so
        // the cost calculator charges the cache-read rate.
        let data = r#"{"object":"chat.completion.chunk","choices":[],
            "usage":{"prompt_tokens":200,"completion_tokens":3,
            "prompt_tokens_details":{"cached_tokens":150}}}"#;
        let mut t = OpenAiStreamTransformer::new();
        let chunks = t.transform(&frame(data));

        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::Usage { usage, .. } => {
                assert_eq!(usage.input_tokens, 200);
                assert_eq!(usage.output_tokens, 3);
                assert_eq!(
                    usage.cache_read_input_tokens,
                    Some(150),
                    "cached_tokens → cache_read_input_tokens"
                );
                assert_eq!(usage.cache_creation_input_tokens, None);
            }
            other => panic!("expected StreamChunk::Usage, got {other:?}"),
        }
    }

    #[test]
    fn non_final_chunk_without_usage_yields_no_usage_chunk() {
        // Regular content delta must NOT carry usage — deserialization of a
        // missing `usage` key yields None, no Usage chunk emitted.
        let data = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#;
        let mut t = OpenAiStreamTransformer::new();
        let chunks = t.transform(&frame(data));

        assert!(
            !chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::Usage { .. })),
            "no Usage chunk should be emitted for a content-only delta"
        );
    }
}
