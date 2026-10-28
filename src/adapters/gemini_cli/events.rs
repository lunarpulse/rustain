//! Cloud Code Assist SSE frame → `StreamChunk` mapping, adapted from oh-my-pi (`ai/src/providers/google-gemini-cli.ts`, `ai/src/providers/google-shared.ts`).
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use crate::domain::models::{ProviderReplayItem, StopReason, StreamChunk, UsageInfo};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const THOUGHT_ONLY: &str =
    "Cloud Code Assist API returned a thought-only response without final output";
const EMPTY_RESPONSE: &str = "Cloud Code Assist API returned an empty response";
const NO_FINISH_REASON: &str = "Cloud Code Assist stream ended without a finish reason (connection dropped or response truncated)";

/// Process-wide tool-call id counter shared by every Gemini stream.
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One captured response part, in arrival order.
#[derive(Debug)]
enum Part {
    Thinking {
        text: String,
        signature: Option<String>,
    },
    Text {
        text: String,
        signature: Option<String>,
    },
    Call {
        name: String,
        args: Value,
        signature: Option<String>,
        id: String,
    },
}

impl Part {
    fn to_replay(&self) -> Value {
        let mut map = Map::new();
        let signature = match self {
            Self::Thinking { text, signature } => {
                map.insert("thought".into(), Value::Bool(true));
                map.insert("text".into(), Value::String(text.clone()));
                signature
            }
            Self::Text { text, signature } => {
                map.insert("text".into(), Value::String(text.clone()));
                signature
            }
            Self::Call {
                name,
                args,
                signature,
                id,
            } => {
                map.insert("functionCall".into(), json!({ "name": name, "args": args }));
                map.insert("callId".into(), Value::String(id.clone()));
                signature
            }
        };
        if let Some(signature) = signature {
            map.insert("thoughtSignature".into(), Value::String(signature.clone()));
        }
        Value::Object(map)
    }
}

/// Mapped `finishReason`; tool promotion is applied at finish.
#[derive(Debug)]
enum Reason {
    Stop,
    Length,
    Error(String),
}

/// End of one attempt.
#[derive(Debug)]
pub(crate) enum Finish {
    /// Success: released text, then `ProviderReplay`, then `Usage` (if seen), then `TurnComplete`.
    Done(Vec<StreamChunk>),
    /// Empty-response retry candidate; nothing visible was forwarded by this attempt.
    Empty,
    /// Terminal failure: forward `pending`, then fail with `message`.
    Error {
        pending: Vec<StreamChunk>,
        message: String,
    },
}

/// Per-attempt mapper from parsed `data:` frames to chunks.
#[derive(Debug)]
pub(crate) struct Mapper {
    model: String,
    flash_leak: bool,
    tool_names: HashSet<String>,
    parts: Vec<Part>,
    /// The last part is an open thinking/text block.
    block_open: bool,
    buffering: bool,
    text_buffer: String,
    buffered_signature: Option<String>,
    reason: Reason,
    saw_finish: bool,
    usage: Option<UsageInfo>,
    /// Visible text stays held while it is whitespace-only and nothing meaningful arrived.
    holding_text: bool,
    held_text: String,
    /// Leading whitespace-only thinking is held likewise until non-blank thinking arrives.
    thinking_started: bool,
    held_thinking: String,
}

impl Mapper {
    /// `model` = options.model (replay stamp); `flash_leak` = profile.flash_leak; `tool_names` = declared tool names (planning-leak classifier).
    pub(crate) fn new(model: &str, flash_leak: bool, tool_names: HashSet<String>) -> Self {
        Self {
            model: model.to_owned(),
            flash_leak,
            tool_names,
            parts: Vec::new(),
            block_open: false,
            buffering: false,
            text_buffer: String::new(),
            buffered_signature: None,
            reason: Reason::Stop,
            saw_finish: false,
            usage: None,
            holding_text: true,
            held_text: String::new(),
            thinking_started: false,
            held_thinking: String::new(),
        }
    }

    /// Chunks to forward now. `Err(text)` ends the stream with that exact text.
    pub(crate) fn frame(&mut self, value: &Value) -> Result<Vec<StreamChunk>, String> {
        let error = &value["error"];
        if truthy(error) {
            let detail = non_empty_str(&error["message"])
                .or_else(|| non_empty_str(&error["status"]))
                .unwrap_or("unknown error");
            return Err(format!("Cloud Code Assist stream error: {detail}"));
        }
        let response = &value["response"];
        let mut out = Vec::new();
        if !truthy(response) {
            return Ok(out);
        }
        let candidates = response["candidates"].as_array();
        if candidates.is_none_or(|list| list.is_empty())
            && let Some(reason) = non_empty_str(&response["promptFeedback"]["blockReason"])
        {
            return Err(
                match non_empty_str(&response["promptFeedback"]["blockReasonMessage"]) {
                    Some(detail) => format!("Request blocked by Google ({reason}): {detail}"),
                    None => format!("Request blocked by Google ({reason})"),
                },
            );
        }

        let candidate = candidates.and_then(|list| list.first());
        if let Some(candidate) = candidate {
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    self.part(part, &mut out);
                }
            }
            if let Some(reason) = non_empty_str(&candidate["finishReason"]) {
                self.saw_finish = true;
                self.reason = match reason {
                    "STOP" => Reason::Stop,
                    "MAX_TOKENS" => Reason::Length,
                    other => {
                        Reason::Error(format!("Generation failed with finish reason: {other}"))
                    }
                };
            }
        }

        let metadata = &response["usageMetadata"];
        if truthy(metadata) {
            self.usage = Some(usage(metadata));
        }
        Ok(out)
    }

    /// End of this attempt's body (EOF or `[DONE]`).
    pub(crate) fn finish(&mut self) -> Finish {
        let mut out = Vec::new();
        if self.buffering && !self.text_buffer.is_empty() {
            let buffered = consume_planning_buffer(&self.text_buffer, &self.tool_names, true);
            let signature = self.buffered_signature.take();
            if let Buffered::Plain(text) | Buffered::Leak(text) = buffered {
                self.emit_text(&text, signature.as_deref(), &mut out);
            }
        }
        self.buffering = false;
        self.text_buffer.clear();
        self.buffered_signature = None;
        self.block_open = false;

        let has_calls = self.parts.iter().any(|p| matches!(p, Part::Call { .. }));
        let meaningful = self.parts.iter().any(|p| match p {
            Part::Call { .. } => true,
            Part::Text { text, .. } => !is_blank(text),
            Part::Thinking { .. } => false,
        });
        let thinking_output = self.parts.iter().any(|p| match p {
            Part::Thinking { text, signature } => !is_blank(text) || signature.is_some(),
            _ => false,
        });

        let stop_reason = match &self.reason {
            Reason::Error(message) => {
                return Finish::Error {
                    pending: out,
                    message: message.clone(),
                };
            }
            _ if has_calls => StopReason::ToolUse,
            Reason::Stop => StopReason::EndTurn,
            Reason::Length => StopReason::MaxTokens,
        };
        if !meaningful {
            if thinking_output {
                return Finish::Error {
                    pending: out,
                    message: THOUGHT_ONLY.into(),
                };
            }
            if matches!(self.reason, Reason::Stop) {
                return Finish::Empty;
            }
            return Finish::Error {
                pending: out,
                message: EMPTY_RESPONSE.into(),
            };
        }
        if !self.saw_finish {
            return Finish::Error {
                pending: out,
                message: NO_FINISH_REASON.into(),
            };
        }

        let parts: Vec<Value> = std::mem::take(&mut self.parts)
            .iter()
            .map(Part::to_replay)
            .collect();
        if !parts.is_empty() {
            let mut payload = Map::new();
            payload.insert(super::REPLAY_KEY.into(), Value::Array(parts));
            out.push(StreamChunk::ProviderReplay {
                item: ProviderReplayItem {
                    provider: super::PROVIDER_KIND.into(),
                    model: self.model.clone(),
                    payload: Arc::new(Value::Object(payload)),
                },
            });
        }
        if let Some(usage) = self.usage.take() {
            out.push(StreamChunk::Usage {
                usage,
                session_id: None,
            });
        }
        out.push(StreamChunk::TurnComplete { stop_reason });
        Finish::Done(out)
    }

    fn part(&mut self, part: &Value, out: &mut Vec<StreamChunk>) {
        let signature = non_empty_str(&part["thoughtSignature"]);
        let call = &part["functionCall"];
        match part["text"].as_str() {
            Some(text) if !text.is_empty() => {
                if part["thought"] == Value::Bool(true) {
                    self.emit_thinking(text, signature, out);
                } else {
                    self.visible(text, signature, out);
                }
            }
            Some(_) if signature.is_some() && !truthy(call) => {
                if let Some((_, slot)) = self.block_mut() {
                    retain_signature(slot, signature);
                }
            }
            _ => {}
        }
        if truthy(call) {
            self.call(call, signature, out);
        }
    }

    fn visible(&mut self, text: &str, signature: Option<&str>, out: &mut Vec<StreamChunk>) {
        if self.buffering {
            self.text_buffer.push_str(text);
            retain_signature(&mut self.buffered_signature, signature);
        } else if self.flash_leak && js_trim_start(text).starts_with('{') {
            self.buffering = true;
            self.text_buffer = text.to_owned();
            self.buffered_signature = signature.map(str::to_owned);
        } else {
            self.emit_text(text, signature, out);
        }

        if self.buffering {
            match consume_planning_buffer(&self.text_buffer, &self.tool_names, false) {
                Buffered::Incomplete => {}
                Buffered::Plain(released) | Buffered::Leak(released) => {
                    self.buffering = false;
                    self.text_buffer.clear();
                    let signature = self.buffered_signature.take();
                    self.emit_text(&released, signature.as_deref(), out);
                }
            }
        }
    }

    fn call(&mut self, call: &Value, signature: Option<&str>, out: &mut Vec<StreamChunk>) {
        self.block_open = false;
        self.buffering = false;
        self.text_buffer.clear();
        self.buffered_signature = None;

        let name = call["name"].as_str().unwrap_or("");
        let provided = non_empty_str(&call["id"]).filter(|provided| {
            !self
                .parts
                .iter()
                .any(|p| matches!(p, Part::Call { id, .. } if id == provided))
        });
        let id = match provided {
            Some(id) => id.to_owned(),
            None => next_tool_call_id(if name.is_empty() { "tool" } else { name }),
        };
        let args = match &call["args"] {
            Value::Null => json!({}),
            args => args.clone(),
        };
        self.parts.push(Part::Call {
            name: name.to_owned(),
            args: args.clone(),
            signature: signature.map(str::to_owned),
            id: id.clone(),
        });
        if self.holding_text {
            self.release_held_text(out);
        }
        out.push(StreamChunk::ToolUse {
            id,
            name: name.to_owned(),
            input: args,
        });
    }

    fn emit_text(&mut self, delta: &str, signature: Option<&str>, out: &mut Vec<StreamChunk>) {
        if delta.is_empty() {
            return;
        }
        self.open_block(false);
        if let Some((text, slot)) = self.block_mut() {
            text.push_str(delta);
            retain_signature(slot, signature);
        }
        if self.holding_text {
            if is_blank(delta) {
                self.held_text.push_str(delta);
                return;
            }
            self.release_held_text(out);
        }
        out.push(StreamChunk::Text {
            content: delta.to_owned(),
            parent_tool_use_id: None,
        });
    }

    fn emit_thinking(&mut self, delta: &str, signature: Option<&str>, out: &mut Vec<StreamChunk>) {
        self.open_block(true);
        if let Some((text, slot)) = self.block_mut() {
            text.push_str(delta);
            retain_signature(slot, signature);
        }
        if self.thinking_started {
            out.push(StreamChunk::Thinking {
                content: delta.to_owned(),
                parent_tool_use_id: None,
            });
        } else if is_blank(delta) {
            self.held_thinking.push_str(delta);
        } else {
            self.thinking_started = true;
            let mut content = std::mem::take(&mut self.held_thinking);
            content.push_str(delta);
            out.push(StreamChunk::Thinking {
                content,
                parent_tool_use_id: None,
            });
        }
    }

    fn release_held_text(&mut self, out: &mut Vec<StreamChunk>) {
        self.holding_text = false;
        if !self.held_text.is_empty() {
            out.push(StreamChunk::Text {
                content: std::mem::take(&mut self.held_text),
                parent_tool_use_id: None,
            });
        }
    }

    /// Keeps the open block when its kind matches, else opens a new one.
    fn open_block(&mut self, thinking: bool) {
        let reuse = self.block_open
            && matches!(
                (self.parts.last(), thinking),
                (Some(Part::Thinking { .. }), true) | (Some(Part::Text { .. }), false)
            );
        if reuse {
            return;
        }
        self.parts.push(if thinking {
            Part::Thinking {
                text: String::new(),
                signature: None,
            }
        } else {
            Part::Text {
                text: String::new(),
                signature: None,
            }
        });
        self.block_open = true;
    }

    fn block_mut(&mut self) -> Option<(&mut String, &mut Option<String>)> {
        if !self.block_open {
            return None;
        }
        match self.parts.last_mut() {
            Some(Part::Thinking { text, signature } | Part::Text { text, signature }) => {
                Some((text, signature))
            }
            _ => None,
        }
    }
}

/// omp `mapGoogleUsage` → rustain usage split.
pub(crate) fn usage(metadata: &Value) -> UsageInfo {
    let candidates = js_number(&metadata["candidatesTokenCount"]);
    let thoughts = js_number(&metadata["thoughtsTokenCount"]);
    let total = js_number(&metadata["totalTokenCount"]);
    let reported_prompt = js_number(&metadata["promptTokenCount"]);
    let prompt = if reported_prompt != 0.0 {
        reported_prompt
    } else {
        (total - candidates - thoughts).max(0.0)
    };
    let cache_read = js_number(&metadata["cachedContentTokenCount"]).min(prompt);
    // `as u32` saturates (negative → 0).
    let cache_read_tokens = cache_read as u32;
    let thought_tokens = thoughts as u32;
    UsageInfo {
        input_tokens: (prompt - cache_read) as u32,
        output_tokens: (candidates + thoughts) as u32,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: (cache_read > 0.0).then_some(cache_read_tokens),
        reasoning_tokens: (thoughts > 0.0).then_some(thought_tokens),
    }
}

fn next_tool_call_id(name: &str) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let counter = TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{name}_{millis}_{counter}")
}

/// omp `retainThoughtSignature`: keep the last non-empty signature.
fn retain_signature(slot: &mut Option<String>, incoming: Option<&str>) {
    if let Some(incoming) = incoming.filter(|s| !s.is_empty()) {
        *slot = Some(incoming.to_owned());
    }
}

/// JS truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn non_empty_str(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}

/// `value || 0` for numeric fields.
fn js_number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

/// JS `String.prototype.trim` whitespace (Unicode White_Space minus NEL, plus BOM).
fn is_js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

fn js_trim_start(text: &str) -> &str {
    text.trim_start_matches(is_js_space)
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_space)
}

fn is_blank(text: &str) -> bool {
    js_trim(text).is_empty()
}

/// JS `string.length` (UTF-16 code units).
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

#[derive(Debug, PartialEq, Eq)]
enum Buffered {
    Incomplete,
    Plain(String),
    Leak(String),
}

/// omp `isPlanningLeakPrefix`.
fn is_planning_leak_prefix(text: &str) -> bool {
    let trimmed = js_trim_start(text);
    let Some(after_open) = trimmed.strip_prefix('{') else {
        return false;
    };
    let short = || js_len(trimmed) <= 100;
    let after_brace = js_trim_start(after_open);
    if after_brace.is_empty() {
        return short();
    }
    let Some(key_and_rest) = after_brace.strip_prefix('"') else {
        return false;
    };
    let Some(end) = key_and_rest.find('"') else {
        return "thought".starts_with(key_and_rest) && short();
    };
    if &key_and_rest[..end] != "thought" {
        return false;
    }
    let after_key = js_trim_start(&key_and_rest[end + 1..]);
    if after_key.is_empty() {
        return short();
    }
    after_key.starts_with(':')
}

/// omp `isPlanningLeakObject`.
fn is_planning_leak_object(parsed: &Value, tool_names: &HashSet<String>) -> bool {
    let Some(record) = parsed.as_object() else {
        return false;
    };
    let has_thought = record.get("thought").is_some_and(Value::is_string);
    let is_tool = record
        .get("call")
        .and_then(Value::as_str)
        .is_some_and(|call| tool_names.contains(call));
    let has_tool_signature = record.contains_key("_i")
        || record.contains_key("paths")
        || record.contains_key("command")
        || (record.contains_key("path") && record.contains_key("content"));
    has_thought || is_tool || has_tool_signature
}

/// Substring leak test used where the object cannot be parsed.
fn has_leak_keys(text: &str, tool_names: &HashSet<String>) -> bool {
    text.contains("\"thought\"")
        || tool_names
            .iter()
            .any(|name| text.contains(&format!("\"{name}\"")))
        || text.contains("\"_i\"")
        || text.contains("\"paths\"")
        || text.contains("\"command\"")
        || (text.contains("\"path\"") && text.contains("\"content\""))
}

/// omp `splitLeadingJsonObject` / `splitLeadingJsonObjectIgnoringQuotes`: `(json_text, rest)`.
fn split_leading_json_object(text: &str, respect_quotes: bool) -> Option<(&str, &str)> {
    let trimmed = js_trim_start(text);
    if !trimmed.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in trimmed.bytes().enumerate() {
        if respect_quotes {
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            if byte == b'"' {
                in_string = true;
                continue;
            }
        }
        if byte == b'{' {
            depth += 1;
        } else if byte == b'}' {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some((&trimmed[..=index], &trimmed[index + 1..]));
            }
        }
    }
    None
}

/// omp `consumePlanningBuffer`.
fn consume_planning_buffer(text: &str, tool_names: &HashSet<String>, is_final: bool) -> Buffered {
    if !is_planning_leak_prefix(text) {
        return Buffered::Plain(text.to_owned());
    }
    let leading =
        split_leading_json_object(text, true).or_else(|| split_leading_json_object(text, false));
    let Some((json_text, rest)) = leading else {
        if !is_final {
            return Buffered::Incomplete;
        }
        // EOF without a closing brace: a leak signature discards the whole buffer.
        if has_leak_keys(js_trim(text), tool_names) {
            return Buffered::Leak(String::new());
        }
        return Buffered::Plain(text.to_owned());
    };
    match serde_json::from_str::<Value>(json_text) {
        Ok(parsed) if is_planning_leak_object(&parsed, tool_names) => {
            Buffered::Leak(rest.to_owned())
        }
        Ok(_) => Buffered::Plain(text.to_owned()),
        Err(_) if has_leak_keys(json_text, tool_names) => Buffered::Leak(rest.to_owned()),
        // An unparseable leading object is not safe to strip.
        Err(_) => Buffered::Plain(text.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper(flash_leak: bool) -> Mapper {
        Mapper::new(
            "gemini-test",
            flash_leak,
            HashSet::from(["read".to_owned()]),
        )
    }

    fn frame_parts(parts: Value) -> Value {
        json!({ "response": { "candidates": [{ "content": { "role": "model", "parts": parts } }] } })
    }

    fn finish_frame(reason: &str) -> Value {
        json!({ "response": { "candidates": [{ "finishReason": reason }] } })
    }

    fn feed(m: &mut Mapper, value: Value) -> Vec<StreamChunk> {
        m.frame(&value).expect("frame ok")
    }

    fn texts(chunks: &[StreamChunk]) -> Vec<String> {
        chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::Text { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    fn done(finish: Finish) -> Vec<StreamChunk> {
        match finish {
            Finish::Done(chunks) => chunks,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    fn error(finish: Finish) -> (Vec<StreamChunk>, String) {
        match finish {
            Finish::Error { pending, message } => (pending, message),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    fn stop_reason(chunks: &[StreamChunk]) -> Option<StopReason> {
        chunks.iter().find_map(|c| match c {
            StreamChunk::TurnComplete { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        })
    }

    fn replay(chunks: &[StreamChunk]) -> Option<Value> {
        chunks.iter().find_map(|c| match c {
            StreamChunk::ProviderReplay { item } => Some(item.payload.as_ref().clone()),
            _ => None,
        })
    }

    fn tool_ids(chunks: &[StreamChunk]) -> Vec<String> {
        chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn maps_parts_in_arrival_order() {
        let mut m = mapper(false);
        let chunks = feed(
            &mut m,
            frame_parts(json!([
                { "thought": true, "text": "plan" },
                { "text": "hello" },
                { "functionCall": { "id": "c1", "name": "read", "args": { "p": 1 } } },
                { "text": "after" }
            ])),
        );
        assert_eq!(chunks.len(), 4);
        assert!(
            matches!(&chunks[0], StreamChunk::Thinking { content, parent_tool_use_id: None } if content == "plan")
        );
        assert!(
            matches!(&chunks[1], StreamChunk::Text { content, parent_tool_use_id: None } if content == "hello")
        );
        assert!(
            matches!(&chunks[2], StreamChunk::ToolUse { id, name, input } if id == "c1" && name == "read" && *input == json!({"p": 1}))
        );
        assert!(matches!(&chunks[3], StreamChunk::Text { content, .. } if content == "after"));
        // A signature alone does not make a part thinking.
        let chunks = feed(
            &mut m,
            frame_parts(json!([{ "text": "x", "thoughtSignature": "U0lH" }])),
        );
        assert_eq!(texts(&chunks), ["x"]);
    }

    #[test]
    fn missing_and_duplicate_tool_ids_are_generated() {
        let mut m = mapper(false);
        let chunks = feed(
            &mut m,
            frame_parts(json!([
                { "functionCall": { "name": "read" } },
                { "functionCall": { "id": "dup", "name": "read", "args": null } },
                { "functionCall": { "id": "dup", "name": "read" } },
                { "functionCall": { "id": "" } }
            ])),
        );
        let ids = tool_ids(&chunks);
        assert!(ids[0].starts_with("read_"), "{ids:?}");
        assert_eq!(ids[1], "dup");
        assert!(ids[2].starts_with("read_") && ids[2] != ids[0], "{ids:?}");
        assert!(ids[3].starts_with("tool_"), "{ids:?}");
        assert!(matches!(&chunks[1], StreamChunk::ToolUse { input, .. } if *input == json!({})));
        assert!(
            matches!(&chunks[3], StreamChunk::ToolUse { name, input, .. } if name.is_empty() && *input == json!({}))
        );
        let counter = |id: &str| id.rsplit('_').next().and_then(|n| n.parse::<u64>().ok());
        assert!(counter(&ids[2]) > counter(&ids[0]));
    }

    #[test]
    fn finish_reason_mapping() {
        let mut m = mapper(false);
        feed(&mut m, frame_parts(json!([{ "text": "hi" }])));
        feed(&mut m, finish_frame("STOP"));
        assert_eq!(stop_reason(&done(m.finish())), Some(StopReason::EndTurn));

        let mut m = mapper(false);
        feed(&mut m, frame_parts(json!([{ "text": "hi" }])));
        feed(&mut m, finish_frame("MAX_TOKENS"));
        assert_eq!(stop_reason(&done(m.finish())), Some(StopReason::MaxTokens));

        for reason in ["STOP", "MAX_TOKENS"] {
            let mut m = mapper(false);
            feed(
                &mut m,
                frame_parts(json!([{ "functionCall": { "name": "read" } }])),
            );
            feed(&mut m, finish_frame(reason));
            assert_eq!(stop_reason(&done(m.finish())), Some(StopReason::ToolUse));
        }

        let mut m = mapper(false);
        feed(
            &mut m,
            frame_parts(json!([{ "functionCall": { "name": "read" } }])),
        );
        feed(&mut m, finish_frame("SAFETY"));
        let (pending, message) = error(m.finish());
        assert!(pending.is_empty());
        assert_eq!(message, "Generation failed with finish reason: SAFETY");

        // The last finish reason wins.
        let mut m = mapper(false);
        feed(&mut m, frame_parts(json!([{ "text": "hi" }])));
        feed(&mut m, finish_frame("RECITATION"));
        feed(&mut m, finish_frame("STOP"));
        assert_eq!(stop_reason(&done(m.finish())), Some(StopReason::EndTurn));
    }

    #[test]
    fn prompt_block_and_in_band_error_texts() {
        let cases = [
            (
                json!({ "error": { "code": 429, "message": "quota", "status": "RESOURCE_EXHAUSTED" } }),
                "Cloud Code Assist stream error: quota",
            ),
            (
                json!({ "error": { "status": "INTERNAL" } }),
                "Cloud Code Assist stream error: INTERNAL",
            ),
            (
                json!({ "error": {} }),
                "Cloud Code Assist stream error: unknown error",
            ),
            (
                json!({ "response": { "promptFeedback": { "blockReason": "SAFETY" } } }),
                "Request blocked by Google (SAFETY)",
            ),
            (
                json!({ "response": { "candidates": [], "promptFeedback": { "blockReason": "OTHER", "blockReasonMessage": "nope" } } }),
                "Request blocked by Google (OTHER): nope",
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(mapper(false).frame(&value).unwrap_err(), expected);
        }
        // Candidates present: feedback is not a block.
        let value = json!({ "response": { "candidates": [{ "content": { "parts": [{ "text": "ok" }] } }], "promptFeedback": { "blockReason": "SAFETY" } } });
        assert_eq!(texts(&mapper(false).frame(&value).unwrap()), ["ok"]);
        // Frames without `response` are skipped.
        assert!(
            mapper(false)
                .frame(&json!({ "traceId": "t" }))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_finish_reason_is_an_error_without_turn_complete() {
        let mut m = mapper(false);
        assert_eq!(
            texts(&feed(&mut m, frame_parts(json!([{ "text": "hi" }])))),
            ["hi"]
        );
        let (pending, message) = error(m.finish());
        assert!(pending.is_empty());
        assert_eq!(message, NO_FINISH_REASON);
    }

    #[test]
    fn usage_split_table() {
        let cases = [
            // prompt, cached, candidates, thoughts, total → input, output, cacheRead, reasoning
            (
                json!({ "promptTokenCount": 100, "candidatesTokenCount": 20, "thoughtsTokenCount": 5, "totalTokenCount": 125 }),
                100,
                25,
                None,
                Some(5),
            ),
            (
                json!({ "promptTokenCount": 100, "cachedContentTokenCount": 40, "candidatesTokenCount": 20, "totalTokenCount": 120 }),
                60,
                20,
                Some(40),
                None,
            ),
            // Missing prompt falls back to total − candidates − thoughts.
            (
                json!({ "candidatesTokenCount": 20, "thoughtsTokenCount": 5, "totalTokenCount": 125 }),
                100,
                25,
                None,
                Some(5),
            ),
            (
                json!({ "promptTokenCount": 0, "candidatesTokenCount": 30, "totalTokenCount": 10 }),
                0,
                30,
                None,
                None,
            ),
            // Cache above prompt is clamped to it.
            (
                json!({ "promptTokenCount": 50, "cachedContentTokenCount": 80, "candidatesTokenCount": 1, "totalTokenCount": 51 }),
                0,
                1,
                Some(50),
                None,
            ),
            (json!({}), 0, 0, None, None),
        ];
        for (metadata, input, output, cache_read, reasoning) in cases {
            let u = usage(&metadata);
            assert_eq!(u.input_tokens, input, "{metadata}");
            assert_eq!(u.output_tokens, output, "{metadata}");
            assert_eq!(u.cache_read_input_tokens, cache_read, "{metadata}");
            assert_eq!(u.cache_creation_input_tokens, None);
            assert_eq!(u.reasoning_tokens, reasoning, "{metadata}");
        }
    }

    #[test]
    fn signature_only_part_signs_current_block_or_is_dropped() {
        let mut m = mapper(false);
        feed(
            &mut m,
            frame_parts(json!([{ "text": "", "thoughtSignature": "QUFB" }])),
        );
        feed(&mut m, frame_parts(json!([{ "text": "x" }])));
        feed(
            &mut m,
            frame_parts(json!([{ "text": "", "thoughtSignature": "QkJC" }])),
        );
        feed(
            &mut m,
            frame_parts(json!([{ "functionCall": { "id": "c", "name": "read" } }])),
        );
        feed(
            &mut m,
            frame_parts(json!([{ "text": "", "thoughtSignature": "Q0ND" }])),
        );
        feed(&mut m, finish_frame("STOP"));
        let payload = replay(&done(m.finish())).expect("replay");
        assert_eq!(
            payload,
            json!({ "gemini_parts": [
                { "text": "x", "thoughtSignature": "QkJC" },
                { "functionCall": { "name": "read", "args": {} }, "callId": "c" }
            ] })
        );
    }

    #[test]
    fn signed_multi_part_turn_replays_exact_parts() {
        let mut m = mapper(false);
        let mut chunks = Vec::new();
        chunks.extend(feed(
            &mut m,
            frame_parts(json!([{ "thought": true, "text": "T", "thoughtSignature": "U0lHMA==" }])),
        ));
        chunks.extend(feed(&mut m, frame_parts(json!([{ "text": "V" }]))));
        chunks.extend(feed(
            &mut m,
            frame_parts(json!([{ "text": "", "thoughtSignature": "U0lHMQ==" }])),
        ));
        chunks.extend(feed(
            &mut m,
            frame_parts(json!([
                { "functionCall": { "name": "a", "args": { "x": 1 } }, "thoughtSignature": "U0lHMg==" },
                { "functionCall": { "name": "b", "args": { "y": "z" } } }
            ])),
        ));
        chunks.extend(feed(
            &mut m,
            json!({ "response": {
                "candidates": [{ "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 3, "totalTokenCount": 13 }
            } }),
        ));
        let ids = tool_ids(&chunks);
        assert_eq!(ids.len(), 2);
        let tail = done(m.finish());
        assert_eq!(tail.len(), 3);
        let StreamChunk::ProviderReplay { item } = &tail[0] else {
            panic!("expected replay first, got {tail:?}");
        };
        assert_eq!(item.provider, super::super::PROVIDER_KIND);
        assert_eq!(item.model, "gemini-test");
        let (call_a, call_b) = (&ids[0], &ids[1]);
        assert_eq!(
            *item.payload,
            json!({ "gemini_parts": [
                { "thought": true, "text": "T", "thoughtSignature": "U0lHMA==" },
                { "text": "V", "thoughtSignature": "U0lHMQ==" },
                { "functionCall": { "name": "a", "args": { "x": 1 } }, "thoughtSignature": "U0lHMg==", "callId": call_a },
                { "functionCall": { "name": "b", "args": { "y": "z" } }, "callId": call_b }
            ] })
        );
        assert!(
            matches!(&tail[1], StreamChunk::Usage { usage, session_id: None } if usage.input_tokens == 10 && usage.output_tokens == 3)
        );
        assert!(matches!(
            &tail[2],
            StreamChunk::TurnComplete {
                stop_reason: StopReason::ToolUse
            }
        ));
    }

    #[test]
    fn whitespace_only_attempt_is_empty_and_forwards_nothing() {
        let mut m = mapper(false);
        assert!(feed(&mut m, frame_parts(json!([{ "text": "  " }]))).is_empty());
        assert!(feed(&mut m, frame_parts(json!([{ "text": "\n" }]))).is_empty());
        assert!(feed(&mut m, finish_frame("STOP")).is_empty());
        assert!(matches!(m.finish(), Finish::Empty));

        // No finish reason and no content is also an empty-retry candidate.
        assert!(matches!(mapper(false).finish(), Finish::Empty));

        // Non-stop finish without content is a terminal empty response.
        let mut m = mapper(false);
        feed(&mut m, finish_frame("MAX_TOKENS"));
        assert_eq!(error(m.finish()).1, EMPTY_RESPONSE);
    }

    #[test]
    fn thought_only_attempt_is_an_error_without_replay() {
        let mut m = mapper(false);
        let chunks = feed(
            &mut m,
            frame_parts(json!([{ "thought": true, "text": "T" }])),
        );
        assert!(matches!(&chunks[0], StreamChunk::Thinking { content, .. } if content == "T"));
        feed(&mut m, finish_frame("STOP"));
        let (pending, message) = error(m.finish());
        assert!(pending.is_empty());
        assert_eq!(message, THOUGHT_ONLY);
    }

    #[test]
    fn leading_whitespace_is_released_before_meaningful_content() {
        let mut m = mapper(false);
        assert!(feed(&mut m, frame_parts(json!([{ "text": "  " }]))).is_empty());
        assert_eq!(
            texts(&feed(&mut m, frame_parts(json!([{ "text": "hi" }])))),
            ["  ", "hi"]
        );
        // Once released, later whitespace flows straight through.
        assert_eq!(
            texts(&feed(&mut m, frame_parts(json!([{ "text": " " }])))),
            [" "]
        );

        let mut m = mapper(false);
        assert!(feed(&mut m, frame_parts(json!([{ "text": "\n" }]))).is_empty());
        let chunks = feed(
            &mut m,
            frame_parts(json!([{ "functionCall": { "id": "c", "name": "read" } }])),
        );
        assert!(matches!(&chunks[0], StreamChunk::Text { content, .. } if content == "\n"));
        assert!(matches!(&chunks[1], StreamChunk::ToolUse { .. }));
    }

    #[test]
    fn flash_leak_classifier() {
        // omp only buffers objects whose first key is `thought`; the other
        // leak markers apply to such objects when `thought` is not a string.
        let cases = [
            (r#"{"thought":"plan the edit"}Hello"#, "Hello"),
            (r#"{"thought":null,"command":"ls -la"}rest"#, "rest"),
            (r#"{"thought":0,"_i":"x"}rest"#, "rest"),
            (r#"{"thought":0,"paths":[]}rest"#, "rest"),
            (r#"{"thought":0,"path":"a.rs","content":"x"}done"#, "done"),
            (r#"{"thought":0,"call":"read","args":{}}after"#, "after"),
            (
                r#"{"thought":0,"call":"other"}kept"#,
                r#"{"thought":0,"call":"other"}kept"#,
            ),
            (
                r#"{"thought":0,"path":"a.rs"}kept"#,
                r#"{"thought":0,"path":"a.rs"}kept"#,
            ),
            (r#"{"command":"ls"}kept"#, r#"{"command":"ls"}kept"#),
            (r#"{"a":1} tail"#, r#"{"a":1} tail"#),
        ];
        for (input, expected) in cases {
            let mut m = mapper(true);
            let chunks = feed(&mut m, frame_parts(json!([{ "text": input }])));
            assert_eq!(texts(&chunks), [expected], "{input}");
        }

        // Split across frames: buffered until the object closes.
        let mut m = mapper(true);
        assert!(feed(&mut m, frame_parts(json!([{ "text": "{\"thou" }]))).is_empty());
        assert_eq!(
            texts(&feed(
                &mut m,
                frame_parts(json!([{ "text": "ght\":\"x\"} ok" }]))
            )),
            [" ok"]
        );

        // Non-flash profiles never buffer.
        let mut m = mapper(false);
        let leak = r#"{"thought":"x"}y"#;
        assert_eq!(
            texts(&feed(&mut m, frame_parts(json!([{ "text": leak }])))),
            [leak]
        );

        // EOF with an incomplete buffer carrying a leak signature discards it.
        let mut m = mapper(true);
        assert_eq!(
            texts(&feed(&mut m, frame_parts(json!([{ "text": "Hi" }])))),
            ["Hi"]
        );
        assert!(
            feed(
                &mut m,
                frame_parts(json!([{ "text": "{\"thought\": \"unterminated" }]))
            )
            .is_empty()
        );
        feed(&mut m, finish_frame("STOP"));
        let tail = done(m.finish());
        assert!(texts(&tail).is_empty());
        assert_eq!(
            replay(&tail),
            Some(json!({ "gemini_parts": [{ "text": "Hi" }] }))
        );

        // EOF with an incomplete plain buffer releases it.
        let mut m = mapper(true);
        assert!(feed(&mut m, frame_parts(json!([{ "text": "{" }]))).is_empty());
        feed(&mut m, finish_frame("STOP"));
        assert_eq!(texts(&done(m.finish())), ["{"]);
    }

    #[test]
    fn planning_buffer_unit_cases() {
        let tools = HashSet::from(["read".to_owned()]);
        assert!(is_planning_leak_prefix("  {"));
        assert!(is_planning_leak_prefix("{\"tho"));
        assert!(is_planning_leak_prefix("{ \"thought\" :"));
        assert!(!is_planning_leak_prefix("{\"a\""));
        assert!(!is_planning_leak_prefix("{1"));
        assert!(!is_planning_leak_prefix("{\"thought\" x"));
        assert!(!is_planning_leak_prefix(&format!("{{{}", " ".repeat(120))));
        // Unescaped quotes fall back to quote-ignoring slicing and substring matching.
        assert_eq!(
            consume_planning_buffer(r#"{"thought":"say "hi""}x"#, &tools, false),
            Buffered::Leak("x".into())
        );
        assert_eq!(
            consume_planning_buffer(r#"{"thought":"a}b"}x"#, &tools, false),
            Buffered::Leak("x".into())
        );
        assert_eq!(
            consume_planning_buffer(r#"{"thought":"open"#, &tools, false),
            Buffered::Incomplete
        );
        assert_eq!(
            consume_planning_buffer(r#"{"thought":"open"#, &tools, true),
            Buffered::Leak(String::new())
        );
        // A non-string `thought` is not a leak.
        assert_eq!(
            consume_planning_buffer(r#"{"thought":1}x"#, &tools, false),
            Buffered::Plain(r#"{"thought":1}x"#.into())
        );
    }
}
