//! Transforms SSE frames into domain `StreamChunk` values for OpenAI-compatible APIs.

use std::sync::Arc;

use crate::domain::models::{StopReason, StreamChunk, UsageInfo};

use super::profile::{InBandError, InBandText};
use super::types::OpenAiStreamEvent;
use crate::adapters::anthropic::sse::SseFrame;
use crate::adapters::github_copilot::plan::display_safe;

/// The code and message of an error object, or the message of a string error:
/// the `error` of a payload that has one (`null` is no error).
pub(super) fn error_parts(value: &serde_json::Value) -> Option<(Option<String>, Option<String>)> {
    value.get("error").and_then(error_member_parts)
}

/// The code and message of an `error` member: an object (`code`, else `type`,
/// and `message`) or a string message. Anything else is no error.
pub(super) fn error_member_parts(
    error: &serde_json::Value,
) -> Option<(Option<String>, Option<String>)> {
    let text = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    match error {
        // An empty or whitespace-only string is a sentinel, not an error.
        serde_json::Value::String(message) if message.trim().is_empty() => None,
        serde_json::Value::String(message) => Some((None, Some(message.clone()))),
        serde_json::Value::Object(_) => Some((
            error
                .get("code")
                .and_then(text)
                .or_else(|| error.get("type").and_then(text)),
            error.get("message").and_then(text),
        )),
        _ => None,
    }
}

/// At most 200 characters of an event payload for the log: the WHOLE payload is
/// made safe by `scrub` (normalised, credentials removed) and only then cut,
/// once, to 200 characters. Nothing is cut before scrubbing: a removable prefix
/// could otherwise push part of a credential across an early cut. Only a
/// malformed frame reaches this.
pub(super) fn log_excerpt(data: &str, scrub: impl Fn(&str) -> String) -> String {
    scrub(data).chars().take(200).collect()
}

#[cfg(test)]
thread_local! {
    /// Frames that needed the second (generic) parse.
    static FALLBACK_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

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
    /// Words an error event; `None` uses the unbranded default text.
    errors: Option<Arc<InBandText>>,
    /// An in-stream error ended the stream.
    failed: bool,
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
            errors: None,
            failed: false,
        }
    }

    /// A transformer that words in-stream errors through `errors`.
    pub fn with_errors(errors: Arc<InBandText>) -> Self {
        Self {
            errors: Some(errors),
            ..Self::new()
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

    /// An error the provider sent inside the stream: a payload whose decoded
    /// JSON has a top-level `error` member (it wins over any `choices` beside
    /// it, whatever the key's spelling on the wire) or an `event: error` frame.
    /// The decoder is finished after it: no later frame produces anything.
    fn error_event(
        &mut self,
        frame: &SseFrame,
        parts: Option<(Option<String>, Option<String>)>,
    ) -> Option<StreamChunk> {
        let named = frame.event == "error";
        if parts.is_none() && !named {
            return None;
        }
        let (code, message) = parts.unwrap_or_default();
        let error = InBandError {
            status: self.errors.as_ref().map_or(200, |e| e.status()),
            content_type: None,
            code: code.as_deref(),
            message: message.as_deref(),
            raw: &frame.data,
            in_stream: true,
        };
        let content = match &self.errors {
            Some(errors) => errors.text(&error),
            None => error.text("The provider", &|part| display_safe(part)),
        };
        self.failed = true;
        self.active_tools.clear();
        Some(StreamChunk::Error { content })
    }

    /// An in-stream error ended the stream: the decoder yields nothing more.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Transform an SSE frame into zero or more domain StreamChunks.
    pub fn transform(&mut self, frame: &SseFrame) -> Vec<StreamChunk> {
        if self.failed {
            return vec![];
        }
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

        // One typed parse on the normal path: `error` is an optional member of
        // the chunk type, so serde's decoded key matching sees an escaped key
        // beside `choices`. Only a frame that fails the typed parse (an
        // error-only payload has no `choices`) is parsed a second time, as a
        // generic tree, to look for the error.
        let event = match serde_json::from_str::<OpenAiStreamEvent>(&frame.data) {
            Ok(event) => {
                let parts = event.error.as_ref().and_then(error_member_parts);
                if let Some(error) = self.error_event(frame, parts) {
                    return vec![error];
                }
                event
            }
            Err(e) => {
                #[cfg(test)]
                FALLBACK_PARSES.with(|n| n.set(n.get() + 1));
                let value = serde_json::from_str::<serde_json::Value>(&frame.data).ok();
                if let Some(error) = self.error_event(frame, value.as_ref().and_then(error_parts)) {
                    return vec![error];
                }
                tracing::warn!(
                    "Failed to parse OpenAI SSE event data: {} — raw: {}",
                    e,
                    log_excerpt(&frame.data, |text| match &self.errors {
                        Some(errors) => errors.scrub(text),
                        None => display_safe(text),
                    })
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

    /// The four lines of a real Copilot `gpt-4.1` answer (the owner's v0.4.0
    /// log, 2026-11-14), verbatim: no top-level `object`.
    const OWNER_LINES: [&str; 4] = [
        r#"{"choices":[],"created":0,"id":"","model":"gpt-4.1-2025-04-14","prompt_filter_results":[{"content_filter_results":{"hate":{"filtered":false,"severity":"safe"},"self_harm":{"filtered":false,"severity":"safe"},"sexual":{"filtered":false,"severity":"safe"},"violence":{"filtered":false,"severity":"safe"}},"prompt_index":0}]}"#,
        r#"{"choices":[{"index":0,"content_filter_results":{},"delta":{"content":"","role":"assistant"}}],"created":1794649681,"id":"chatcmpl-Ek4eXEDaj9CRNMPUZQUFQK9SoiBw5","model":"gpt-4.1-2025-04-14","system_fingerprint":"fp_8c4c3e6d7a"}"#,
        r#"{"choices":[{"index":0,"content_filter_results":{},"delta":{"content":"Hello"}}],"created":1794649681,"id":"chatcmpl-Ek4eXEDaj9CRNMPUZQUFQK9SoiBw5","model":"gpt-4.1-2025-04-14","system_fingerprint":"fp_8c4c3e6d7a"}"#,
        r#"{"choices":[{"finish_reason":"stop","index":0,"content_filter_results":{},"delta":{"content":null}}],"created":1794649681,"id":"chatcmpl-Ek4eXEDaj9CRNMPUZQUFQK9SoiBw5","usage":{"completion_tokens":10,"completion_tokens_details":{"accepted_prediction_tokens":0,"rejected_prediction_tokens":0},"prompt_tokens":1549,"prompt_tokens_details":{"cached_tokens":1408},"total_tokens":1559},"model":"gpt-4.1-2025-04-14","system_fingerprint":"fp_8c4c3e6d7a","copilot_usage":{"token_details":[{"batch_size":1000000,"cost_per_batch":0,"model":"gpt-4.1","token_count":141,"token_type":"input"},{"batch_size":1000000,"cost_per_batch":0,"model":"gpt-4.1","token_count":1408,"token_type":"cache_read"},{"batch_size":1000000,"cost_per_batch":0,"model":"gpt-4.1","token_count":0,"token_type":"cache_write"},{"batch_size":1000000,"cost_per_batch":0,"model":"gpt-4.1","token_count":10,"token_type":"output"}],"total_nano_aiu":0}}"#,
    ];

    fn all(frames: &[SseFrame]) -> Vec<StreamChunk> {
        let mut t = OpenAiStreamTransformer::new();
        frames.iter().flat_map(|f| t.transform(f)).collect()
    }

    #[test]
    fn the_owners_copilot_lines_decode_in_order() {
        let frames: Vec<_> = OWNER_LINES.iter().map(|line| frame(line)).collect();
        let chunks = all(&frames);
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert!(
            matches!(&chunks[0], StreamChunk::Text { content, .. } if content == "Hello"),
            "{chunks:?}"
        );
        assert!(
            matches!(
                &chunks[1],
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn
                }
            ),
            "{chunks:?}"
        );
        match &chunks[2] {
            StreamChunk::Usage { usage, .. } => {
                assert_eq!(usage.input_tokens, 1549);
                assert_eq!(usage.output_tokens, 10);
                assert_eq!(usage.cache_read_input_tokens, Some(1408));
            }
            other => panic!("expected usage, got {other:?}"),
        }
    }

    #[test]
    fn chunks_with_and_without_object_decode_and_meaning_stays_required() {
        for line in OWNER_LINES {
            assert!(
                serde_json::from_str::<OpenAiStreamEvent>(line).is_ok(),
                "{line}"
            );
        }
        for missing in [
            r#"{"id":"x"}"#,
            r#"{"choices":[{"index":0}]}"#,
            r#"{"choices":[{"delta":{"content":"a"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":1}}"#,
        ] {
            assert!(
                serde_json::from_str::<OpenAiStreamEvent>(missing).is_err(),
                "{missing}"
            );
        }
        // Azure: an empty `object` on the annotation chunk, then content chunks.
        let azure_first =
            r#"{"object":"","choices":[],"prompt_filter_results":[{"prompt_index":0}]}"#;
        let content = r#"{"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hi"},"content_filter_results":{}}]}"#;
        let stop = r#"{"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        assert_eq!(
            run(&[azure_first, content, stop]),
            ["text:Hi", "stop:EndTurn"]
        );
        assert!(run(&[r#"{"choices":["#]).is_empty(), "malformed: nothing");
    }

    #[test]
    fn the_log_excerpt_is_bounded_scrubbed_and_never_a_cut_credential() {
        let payload = format!("\u{1b}[2J\u{7}é€{}", "ü".repeat(10_000));
        let excerpt = log_excerpt(&payload, display_safe);
        assert_eq!(excerpt.chars().count(), 200);
        assert!(!excerpt.chars().any(char::is_control), "{excerpt:?}");
        assert_eq!(log_excerpt("short", display_safe), "short");
        // The credential is removed from the whole head before the 200 cut.
        let payload = format!("{} {TOKEN} tail", "a".repeat(190));
        let hide = |text: &str| text.replace(TOKEN, "[redacted]");
        let excerpt = log_excerpt(&payload, hide);
        assert!(!excerpt.contains("SECRET"), "{excerpt}");
    }

    fn error_text(data: &str, event: &str) -> Vec<String> {
        let mut t = OpenAiStreamTransformer::new();
        t.transform(&SseFrame {
            event: event.to_string(),
            data: data.to_string(),
        })
        .into_iter()
        .map(|chunk| match chunk {
            StreamChunk::Error { content } => content,
            other => format!("not an error: {other:?}"),
        })
        .collect()
    }

    #[test]
    fn error_events_are_one_error_and_win_over_choices() {
        let texts = error_text(
            r#"{"error":{"message":"request rejected","code":"zz"}}"#,
            "message",
        );
        assert_eq!(texts.len(), 1);
        assert!(
            texts[0].contains("request rejected") && texts[0].contains("zz"),
            "{texts:?}"
        );
        // A neutral message does not read as transient: the transport's own
        // wording must not make the CHAT budget fire by itself.
        assert!(
            !crate::adapters::responses::events::generic_retryable(&texts[0]),
            "{texts:?}"
        );
        // A string error, a numeric code, and `event: error` with plain data.
        assert!(error_text(r#"{"error":"plain"}"#, "message")[0].ends_with("plain"));
        assert!(
            error_text(r#"{"error":{"code":429,"message":"m"}}"#, "message")[0].contains("429: m")
        );
        assert!(error_text("plain words", "error")[0].contains("plain words"));
        let both = r#"{"error":{"message":"bad"},"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}]}"#;
        let texts = error_text(both, "message");
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("bad"), "{texts:?}");
        // Not errors: a null error, an annotation chunk, a content chunk.
        for data in [
            r#"{"error":null,"choices":[]}"#,
            OWNER_LINES[0],
            r#"{"choices":[{"index":0,"delta":{"content":"error"}}]}"#,
        ] {
            assert!(
                error_text(data, "message")
                    .iter()
                    .all(|t| t.starts_with("not an error")),
                "{data}"
            );
        }
    }

    const TOKEN: &str = "SECRETTOKEN12345";

    fn bound() -> OpenAiStreamTransformer {
        let mut sent = reqwest::header::HeaderMap::new();
        sent.insert(
            "authorization",
            reqwest::header::HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap(),
        );
        OpenAiStreamTransformer::with_errors(Arc::new(InBandText::new(
            super::super::StaticChatProfile::new("http://stub.invalid/v1"),
            sent,
            "m".into(),
            200,
        )))
    }

    fn data_frame(data: &str) -> SseFrame {
        SseFrame {
            event: "message".into(),
            data: data.into(),
        }
    }

    /// CP2-R5: the error key is read from the decoded JSON, whatever its
    /// spelling on the wire.
    #[test]
    fn an_escaped_error_key_is_still_an_error_and_wins_over_choices() {
        let escaped = r#"{"err\u006fr":{"message":"request rejected","c\u006fde":"zz"}}"#;
        let both = r#"{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}],"\u0065rror":{"message":"bad"}}"#;
        for data in [escaped, both] {
            let mut t = OpenAiStreamTransformer::new();
            let chunks = t.transform(&data_frame(data));
            assert_eq!(chunks.len(), 1, "{data}: {chunks:?}");
            assert!(
                matches!(&chunks[0], StreamChunk::Error { .. }),
                "{chunks:?}"
            );
        }
    }

    /// CP2-R2: the decoder itself is finished by an error, whatever follows.
    #[test]
    fn after_an_error_nothing_more_is_decoded() {
        let mut t = bound();
        let tool = tool_delta(0, "call_1", "Bash", r#"{"command":"x"}"#);
        assert!(t.transform(&data_frame(&tool)).is_empty());
        let chunks = t.transform(&data_frame(r#"{"error":{"message":"request rejected"}}"#));
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        assert!(t.failed());
        for later in [
            DONE.to_owned(),
            text_delta("late"),
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_owned(),
            r#"{"error":{"message":"again"}}"#.to_owned(),
        ] {
            assert!(t.transform(&data_frame(&later)).is_empty(), "{later}");
        }
    }

    /// CP2-R3: the parse WARN carries the scrubbed excerpt, never the bearer.
    #[tracing_test::traced_test]
    #[test]
    fn the_parse_warning_never_logs_the_bearer() {
        let mut t = bound();
        let split = format!("{{\"choices\":[ {}\u{1b}[0m{}", &TOKEN[..6], &TOKEN[6..]);
        assert!(t.transform(&data_frame(&split)).is_empty());
        assert!(logs_contain("Failed to parse OpenAI SSE event data"));
        assert!(!logs_contain(TOKEN));
        assert!(logs_contain("[redacted]"));
    }

    /// CP2-R8: removable ANSI prefixes push the credential across any early
    /// cut; nothing is cut before the whole payload is scrubbed.
    #[tracing_test::traced_test]
    #[test]
    fn an_ansi_prefix_cannot_push_a_credential_across_a_cut() {
        let mut t = bound();
        // The credential starts at character 4,090 of the raw payload (77) or far past it.
        for pad in [77, 5000] {
            let payload = format!(
                "{{\"choices\":[ {}{}{TOKEN}",
                "\u{1b}[0m".repeat(1000),
                "x".repeat(pad)
            );
            assert!(t.transform(&data_frame(&payload)).is_empty());
        }
        assert!(logs_contain("Failed to parse OpenAI SSE event data"));
        assert!(
            !logs_contain("SECRET"),
            "a credential fragment reached the log"
        );
        // Directly: the credential starts at character 4,090.
        let payload = format!("{}{}{TOKEN}", "\u{1b}[0m".repeat(1000), "x".repeat(90));
        let hide = |text: &str| display_safe(text).replace(TOKEN, "[redacted]");
        assert!(!log_excerpt(&payload, hide).contains("SECRET"));
    }

    /// CP2-R10: an empty or whitespace-only string `error` (a sentinel some
    /// providers send on every chunk) is no error: the chunk decodes and the
    /// stream continues. An error-only frame with such a value is not fatal
    /// either; `event: error` and non-empty errors still are.
    #[test]
    fn an_empty_string_error_is_not_an_error() {
        for sentinel in [r#""""#, r#""  ""#, "null", "false", "0"] {
            let mut t = OpenAiStreamTransformer::new();
            let chunk = format!(
                r#"{{"choices":[{{"index":0,"delta":{{"content":"Hi"}}}}],"error":{sentinel}}}"#
            );
            let chunks = t.transform(&data_frame(&chunk));
            assert!(
                matches!(&chunks[..], [StreamChunk::Text { content, .. }] if content == "Hi"),
                "{sentinel}: {chunks:?}"
            );
            assert!(!t.failed(), "{sentinel}");
            let later = t.transform(&data_frame(&text_delta("more")));
            assert_eq!(later.len(), 1, "{sentinel}: the stream continues");
        }
        for only in [r#"{"error":""}"#, r#"{"error":"  "}"#] {
            let mut t = OpenAiStreamTransformer::new();
            assert!(t.transform(&data_frame(only)).is_empty(), "{only}");
            assert!(!t.failed(), "{only}");
        }
        // Still fatal: a non-empty string, an object, and `event: error`.
        for fatal in [r#"{"error":"bad"}"#, r#"{"error":{"message":"bad"}}"#] {
            let chunks = OpenAiStreamTransformer::new().transform(&data_frame(fatal));
            assert!(
                matches!(&chunks[..], [StreamChunk::Error { .. }]),
                "{fatal}"
            );
        }
        let mut t = OpenAiStreamTransformer::new();
        let named = SseFrame {
            event: "error".into(),
            data: r#"{"error":""}"#.into(),
        };
        assert_eq!(t.transform(&named).len(), 1);
        assert!(t.failed());
    }

    /// CP2-R9: a normal chunk is ONE typed parse (no generic tree); only a
    /// frame the typed decoder rejects takes the second parse.
    #[test]
    fn normal_chunks_take_the_single_typed_parse_path() {
        let before = FALLBACK_PARSES.with(|n| n.get());
        let mut t = OpenAiStreamTransformer::new();
        for line in OWNER_LINES {
            t.transform(&data_frame(line));
        }
        t.transform(&data_frame(&text_delta("more")));
        assert_eq!(
            FALLBACK_PARSES.with(|n| n.get()),
            before,
            "a normal chunk fell back"
        );
        // An escaped `error` key beside `choices` is found by the typed parse.
        let mut t = OpenAiStreamTransformer::new();
        let both = r#"{"choices":[{"index":0,"delta":{}}],"\u0065rror":{"message":"bad"}}"#;
        assert!(matches!(
            t.transform(&data_frame(both))[..],
            [StreamChunk::Error { .. }]
        ));
        assert_eq!(
            FALLBACK_PARSES.with(|n| n.get()),
            before,
            "error+choices fell back"
        );
        // An error-only frame has no `choices`: exactly one fallback parse.
        let mut t = OpenAiStreamTransformer::new();
        let only = r#"{"err\u006fr":{"message":"bad"}}"#;
        assert!(matches!(
            t.transform(&data_frame(only))[..],
            [StreamChunk::Error { .. }]
        ));
        assert_eq!(FALLBACK_PARSES.with(|n| n.get()), before + 1);
    }
}
