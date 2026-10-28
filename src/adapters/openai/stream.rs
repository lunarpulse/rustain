//! Transforms SSE frames into domain `StreamChunk` values for OpenAI-compatible APIs.

use crate::domain::models::{StopReason, StreamChunk, UsageInfo};

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
    /// The turn produced text, thinking or a tool call.
    output: bool,
    /// A `finish_reason` ended the turn.
    finished: bool,
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
            output: false,
            finished: false,
        }
    }

    /// Ends the turn with `reason`, emitting the accumulated tool calls first
    /// (in index order). A natural stop that produced tool calls finishes as
    /// `ToolUse`: a tool call always means "execute and continue" (omp
    /// openai-completions promotes `stop` → `toolUse`).
    fn finish(&mut self, reason: StopReason, chunks: &mut Vec<StreamChunk>) {
        self.finished = true;
        let mut tools: Vec<_> = self.active_tools.drain().collect();
        tools.sort_unstable_by_key(|(index, _)| *index);
        let called = !tools.is_empty();
        for (_, tool) in tools {
            let input = serde_json::from_str(&tool.arguments_json).unwrap_or_else(|e| {
                tracing::warn!("Failed to parse tool arguments JSON: {}", e);
                serde_json::Value::Object(serde_json::Map::new())
            });
            chunks.push(StreamChunk::ToolUse {
                id: tool.id,
                name: tool.name,
                input,
            });
        }
        let stop_reason = if called && reason == StopReason::EndTurn {
            StopReason::ToolUse
        } else {
            reason
        };
        chunks.push(StreamChunk::TurnComplete { stop_reason });
    }

    /// Transform an SSE frame into zero or more domain StreamChunks.
    pub fn transform(&mut self, frame: &SseFrame) -> Vec<StreamChunk> {
        // `[DONE]` is the protocol's terminal: a turn that produced output but
        // no `finish_reason` completed by server agreement and ends as a
        // default stop; after a `finish_reason` it adds nothing.
        if frame.data.trim() == "[DONE]" {
            let mut chunks = Vec::new();
            if self.output && !self.finished {
                self.finish(StopReason::EndTurn, &mut chunks);
            }
            return chunks;
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
                    self.output = true;
                    chunks.push(StreamChunk::Text {
                        content: text.clone(),
                        parent_tool_use_id: None,
                    });
                }
            }

            // DeepSeek v4 reasoning/thinking content
            if let Some(reasoning) = &delta.reasoning_content {
                if !reasoning.is_empty() {
                    self.output = true;
                    chunks.push(StreamChunk::Thinking {
                        content: reasoning.clone(),
                        parent_tool_use_id: None,
                    });
                }
            }

            // Tool call deltas
            if let Some(tool_calls) = &delta.tool_calls {
                for tc in tool_calls {
                    // `id`, `function.name`, and `function.arguments` are
                    // independently optional fields on each delta once a
                    // tool-call id has established the accumulator. A provider
                    // may coalesce all three into the first delta (ZAI GLM
                    // Coding Plan) or split them across deltas (OpenAI/DeepSeek).
                    // Modeling id and arguments as mutually exclusive phases
                    // drops same-delta arguments; accepting function fragments
                    // before any id would instead emit unusable empty ids.
                    let tool = match self.active_tools.entry(tc.index) {
                        std::collections::hash_map::Entry::Occupied(entry) => {
                            let tool = entry.into_mut();
                            if let Some(id) = &tc.id {
                                if id.is_empty() {
                                    tracing::warn!(
                                        "Received empty tool-call id for index {}",
                                        tc.index
                                    );
                                } else if tool.id != *id {
                                    tracing::warn!("Tool-call id changed for index {}", tc.index);
                                    tool.id = id.clone();
                                    tool.name.clear();
                                    tool.arguments_json.clear();
                                }
                            }
                            tool
                        }
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            let Some(id) = tc.id.as_ref().filter(|id| !id.is_empty()) else {
                                tracing::warn!(
                                    "Received tool-call delta for index {} before tool-call id",
                                    tc.index
                                );
                                continue;
                            };
                            self.output = true;
                            entry.insert(ToolAccumulator {
                                id: id.clone(),
                                name: String::new(),
                                arguments_json: String::new(),
                            })
                        }
                    };
                    if let Some(function) = &tc.function {
                        if let Some(name) = &function.name {
                            tool.name = name.clone();
                        }
                        if let Some(args) = &function.arguments {
                            tool.arguments_json.push_str(args);
                        }
                    }
                }
            }

            // Finish reason — emit the tool calls (if any) and TurnComplete
            if let Some(reason) = &choice.finish_reason {
                match reason.as_str() {
                    "stop" => self.finish(StopReason::EndTurn, &mut chunks),
                    "tool_calls" => self.finish(StopReason::ToolUse, &mut chunks),
                    "length" => {
                        self.finished = true;
                        chunks.push(StreamChunk::TurnComplete {
                            stop_reason: StopReason::MaxTokens,
                        });
                    }
                    _ => {
                        self.finished = true;
                        chunks.push(StreamChunk::TurnComplete {
                            stop_reason: StopReason::EndTurn,
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

    #[test]
    fn zai_coalesced_first_delta_arguments_are_preserved() {
        // ZAI GLM Coding Plan: the first `tool_calls` delta coalesces the call
        // `id`, `function.name`, AND the complete `function.arguments` into one
        // chunk. The previous parser dropped the arguments because it only
        // created a fresh accumulator (with empty args) when `id` was present.
        let first = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_zai_1",
            "type":"function","function":{"name":"get_weather",
            "arguments":"{\"city\":\"Paris\"}"}}]},
            "finish_reason":null}]}"#;
        let terminal = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut t = OpenAiStreamTransformer::new();
        let _ = t.transform(&frame(first));
        let chunks = t.transform(&frame(terminal));

        // ToolUse with the coalesced arguments, then TurnComplete(ToolUse).
        let tool_use = chunks
            .iter()
            .find(|c| matches!(c, StreamChunk::ToolUse { .. }));
        assert!(tool_use.is_some(), "expected a ToolUse chunk");
        match tool_use.unwrap() {
            StreamChunk::ToolUse { id, name, input } => {
                assert_eq!(id, "call_zai_1", "tool id from first delta");
                assert_eq!(name, "get_weather", "function name from first delta");
                assert_eq!(*input, serde_json::json!({"city": "Paris"}));
            }
            other => panic!("expected StreamChunk::ToolUse, got {other:?}"),
        }
        assert!(
            chunks.iter().any(|c| matches!(
                c,
                StreamChunk::TurnComplete {
                    stop_reason: crate::domain::models::StopReason::ToolUse
                }
            )),
            "expected TurnComplete with ToolUse stop reason"
        );
    }

    #[test]
    fn zai_streamed_first_fragment_is_retained_and_completed() {
        // ZAI tool_stream-style chunking: the opening argument fragment `{"`
        // arrives in the same delta as `id`/`name`; later deltas without `id`
        // append the remaining fragments (`city`, `":"`, `Paris"`, `}`). The
        // first fragment must be retained, and the concatenated fragments must
        // parse into the complete JSON object.
        let d1 = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_zai_2",
            "type":"function","function":{"name":"get_weather",
            "arguments":"{\""}}]},"finish_reason":null}]}"#;
        let d2 = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,
            "function":{"arguments":"city"}}]},"finish_reason":null}]}"#;
        let d3 = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,
            "function":{"arguments":"\":\""}}]},"finish_reason":null}]}"#;
        let d4 = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,
            "function":{"arguments":"Paris\""}}]},"finish_reason":null}]}"#;
        let d5 = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,
            "function":{"arguments":"}"}}]},"finish_reason":null}]}"#;
        let terminal = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut t = OpenAiStreamTransformer::new();
        let _ = t.transform(&frame(d1));
        let _ = t.transform(&frame(d2));
        let _ = t.transform(&frame(d3));
        let _ = t.transform(&frame(d4));
        let _ = t.transform(&frame(d5));
        let chunks = t.transform(&frame(terminal));

        let tool_use = chunks
            .iter()
            .find(|c| matches!(c, StreamChunk::ToolUse { .. }));
        assert!(tool_use.is_some(), "expected a ToolUse chunk");
        match tool_use.unwrap() {
            StreamChunk::ToolUse { id, name, input } => {
                assert_eq!(id, "call_zai_2");
                assert_eq!(name, "get_weather");
                assert_eq!(*input, serde_json::json!({"city": "Paris"}));
            }
            other => panic!("expected StreamChunk::ToolUse, got {other:?}"),
        }
        assert!(
            chunks.iter().any(|c| matches!(
                c,
                StreamChunk::TurnComplete {
                    stop_reason: crate::domain::models::StopReason::ToolUse
                }
            )),
            "expected TurnComplete with ToolUse stop reason"
        );
    }

    #[test]
    fn split_delta_arguments_accumulate_unchanged() {
        // Existing OpenAI/DeepSeek behavior: `id`/`name` arrive on the first
        // delta with no arguments; arguments arrive on a later delta. This
        // split-delta path must remain unchanged after the refactor.
        let first = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_split",
            "type":"function","function":{"name":"get_weather"}}]},
            "finish_reason":null}]}"#;
        let args = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,
            "function":{"arguments":"{\"city\":\"Tokyo\"}"}}]},
            "finish_reason":null}]}"#;
        let terminal = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut t = OpenAiStreamTransformer::new();
        let _ = t.transform(&frame(first));
        let _ = t.transform(&frame(args));
        let chunks = t.transform(&frame(terminal));

        let tool_use = chunks
            .iter()
            .find(|c| matches!(c, StreamChunk::ToolUse { .. }));
        assert!(tool_use.is_some(), "expected a ToolUse chunk");
        match tool_use.unwrap() {
            StreamChunk::ToolUse { id, name, input } => {
                assert_eq!(id, "call_split");
                assert_eq!(name, "get_weather");
                assert_eq!(*input, serde_json::json!({"city": "Tokyo"}));
            }
            other => panic!("expected StreamChunk::ToolUse, got {other:?}"),
        }
        assert!(
            chunks.iter().any(|c| matches!(
                c,
                StreamChunk::TurnComplete {
                    stop_reason: crate::domain::models::StopReason::ToolUse
                }
            )),
            "expected TurnComplete with ToolUse stop reason"
        );
    }

    #[test]
    fn function_delta_before_id_is_ignored() {
        // A function-only first delta cannot be correlated to a later tool
        // result. Preserve the previous behavior of not emitting a ToolUse for
        // fragments that arrive before any tool-call id establishes the index.
        let args_before_id = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,
            "function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},
            "finish_reason":null}]}"#;
        let terminal = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut t = OpenAiStreamTransformer::new();
        let _ = t.transform(&frame(args_before_id));
        let chunks = t.transform(&frame(terminal));

        assert!(
            !chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::ToolUse { .. })),
            "function-only delta before id must not emit an uncorrelatable ToolUse"
        );
        assert!(
            chunks.iter().any(|c| matches!(
                c,
                StreamChunk::TurnComplete {
                    stop_reason: crate::domain::models::StopReason::ToolUse
                }
            )),
            "terminal tool_calls still completes the turn"
        );
    }

    #[test]
    fn changed_tool_call_id_resets_accumulated_arguments() {
        // If a provider reuses an index with a different id before the finish
        // chunk, the new call must not concatenate onto the previous call's JSON.
        let first = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_old",
            "type":"function","function":{"name":"get_weather",
            "arguments":"{\"city\":\"Old\"}"}}]},"finish_reason":null}]}"#;
        let changed = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_new",
            "type":"function","function":{"name":"get_weather",
            "arguments":"{\"city\":\"Paris\"}"}}]},"finish_reason":null}]}"#;
        let terminal = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut t = OpenAiStreamTransformer::new();
        let _ = t.transform(&frame(first));
        let _ = t.transform(&frame(changed));
        let chunks = t.transform(&frame(terminal));

        let tool_use = chunks
            .iter()
            .find(|c| matches!(c, StreamChunk::ToolUse { .. }));
        assert!(tool_use.is_some(), "expected a ToolUse chunk");
        match tool_use.unwrap() {
            StreamChunk::ToolUse { id, name, input } => {
                assert_eq!(id, "call_new");
                assert_eq!(name, "get_weather");
                assert_eq!(*input, serde_json::json!({"city": "Paris"}));
            }
            other => panic!("expected StreamChunk::ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn empty_tool_call_id_cannot_start_accumulator() {
        let empty_id = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,"id":"",
            "type":"function","function":{"name":"get_weather",
            "arguments":"{\"city\":\"Paris\"}"}}]},"finish_reason":null}]}"#;
        let terminal = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut t = OpenAiStreamTransformer::new();
        let _ = t.transform(&frame(empty_id));
        let chunks = t.transform(&frame(terminal));

        assert!(
            !chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::ToolUse { .. })),
            "empty id must not create an uncorrelatable ToolUse"
        );
    }

    const DONE: &str = "[DONE]";

    fn text_delta(text: &str) -> String {
        serde_json::json!({"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"content":text},"finish_reason":null}]})
        .to_string()
    }
    fn tool_delta(index: usize, id: &str, name: &str, arguments: &str) -> String {
        serde_json::json!({"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"tool_calls":[{"index":index,"id":id,"type":"function",
            "function":{"name":name,"arguments":arguments}}]},"finish_reason":null}]})
        .to_string()
    }
    fn finish_delta(reason: &str) -> String {
        serde_json::json!({"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{},"finish_reason":reason}]})
        .to_string()
    }

    /// Every chunk of `frames`, rendered compactly.
    fn run(frames: &[&str]) -> Vec<String> {
        let mut t = OpenAiStreamTransformer::new();
        frames
            .iter()
            .flat_map(|data| t.transform(&frame(data)))
            .filter_map(|chunk| match chunk {
                StreamChunk::Text { content, .. } => Some(format!("text:{content}")),
                StreamChunk::ToolUse { id, input, .. } => Some(format!("tool:{id}:{input}")),
                StreamChunk::TurnComplete { stop_reason } => Some(format!("stop:{stop_reason:?}")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn done_after_output_without_finish_reason_ends_the_turn_once() {
        let text = text_delta("hi");
        assert_eq!(
            run(&[text.as_str(), DONE, DONE]),
            ["text:hi", "stop:EndTurn"],
            "[DONE] is a terminal; a repeated [DONE] adds nothing"
        );
    }

    #[test]
    fn done_after_tool_deltas_emits_the_calls_in_index_order_and_tool_use() {
        let second = tool_delta(1, "call_b", "read", r#"{"path":"b"}"#);
        let first = tool_delta(0, "call_a", "read", r#"{"path":"a"}"#);
        assert_eq!(
            run(&[second.as_str(), first.as_str(), DONE]),
            [
                r#"tool:call_a:{"path":"a"}"#,
                r#"tool:call_b:{"path":"b"}"#,
                "stop:ToolUse"
            ]
        );
    }

    #[test]
    fn stop_with_tool_calls_finishes_as_tool_use() {
        let text = text_delta("let me look");
        let call = tool_delta(0, "call_a", "read", r#"{"path":"a"}"#);
        let stop = finish_delta("stop");
        assert_eq!(
            run(&[text.as_str(), call.as_str(), stop.as_str(), DONE]),
            [
                "text:let me look",
                r#"tool:call_a:{"path":"a"}"#,
                "stop:ToolUse"
            ]
        );
        // Without tool calls a stop stays a stop; `length` is unchanged.
        assert_eq!(
            run(&[text.as_str(), stop.as_str(), DONE]),
            ["text:let me look", "stop:EndTurn"]
        );
        let length = finish_delta("length");
        assert_eq!(
            run(&[call.as_str(), length.as_str(), DONE]),
            ["stop:MaxTokens"]
        );
    }

    #[test]
    fn done_after_a_finish_reason_or_without_output_adds_nothing() {
        let text = text_delta("hi");
        let calls = finish_delta("tool_calls");
        let usage = r#"{"object":"chat.completion.chunk","choices":[],
            "usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
        assert_eq!(
            run(&[text.as_str(), calls.as_str(), usage, DONE]),
            ["text:hi", "stop:ToolUse"]
        );
        let role = r#"{"object":"chat.completion.chunk","choices":[
            {"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        assert!(run(&[role, DONE]).is_empty(), "no output: no terminal");
    }
}
