//! Responses event mapping, adapted from oh-my-pi's OpenAI/Codex providers
//! (generic dialect: openai-shared.ts processResponsesStream, openai-responses.ts
//! replay-unsafe events, error/flags.ts and error/retryable.ts transient rules).
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

use super::Dialect;
use crate::domain::models::{ProviderReplayItem, StopReason, StreamChunk, UsageInfo};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

#[derive(Debug)]
pub struct EventError {
    pub message: String,
    pub retryable: bool,
    pub whitespace_loop: bool,
}

#[derive(Default)]
struct Tool {
    item_id: Option<String>,
    call_id: String,
    name: String,
    arguments: String,
    arguments_done: bool,
    emitted: bool,
}

struct WhitespaceRun {
    item_id: String,
    output_index: Option<i64>,
    events: usize,
    chars: usize,
    first_sequence: Option<i64>,
    last_sequence: Option<i64>,
}

/// One attempt's state only. The transport owns credentials, timeouts, retries,
/// metadata capture and nonterminal EOF detection, never this mapper.
#[derive(Default)]
pub struct EventMapper {
    tools: Vec<Tool>,
    by_id: HashMap<String, usize>,
    by_index: HashMap<i64, usize>,
    emitted_calls: HashSet<String>,
    current: Option<usize>,
    whitespace: Option<WhitespaceRun>,
    terminal: bool,
    committed: bool,
    tool_emitted: bool,
    /// `(provider, model)` stamping captured reasoning; `None` captures nothing.
    replay_owner: Option<(String, String)>,
    /// This attempt's reduced reasoning items, released only at a terminal event.
    replay: Vec<Value>,
    dialect: Dialect,
}

/// omp `isOpenAIResponsesReplayUnsafeEvent`: once one is seen, the generic
/// transport never replays the attempt.
pub fn replay_unsafe(event: &Value) -> bool {
    match event["type"].as_str() {
        Some(
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta",
        ) => event["delta"]
            .as_str()
            .is_some_and(|delta| !delta.is_empty()),
        Some("response.reasoning_summary_part.done" | "response.output_item.done") => true,
        _ => false,
    }
}

/// omp `isOpenAIResponsesProgressEvent`: only these count as the generic first
/// event or restart its idle timer.
pub(super) fn progress(event: &Value) -> bool {
    matches!(
        event["type"].as_str(),
        Some(
            "response.created"
                | "response.output_item.added"
                | "response.reasoning_summary_part.added"
                | "response.reasoning_summary_text.delta"
                | "response.reasoning_summary_text.done"
                | "response.reasoning_summary_part.done"
                | "response.reasoning_text.delta"
                | "response.content_part.added"
                | "response.output_text.delta"
                | "response.refusal.delta"
                | "response.function_call_arguments.delta"
                | "response.function_call_arguments.done"
                | "response.custom_tool_call_input.delta"
                | "response.custom_tool_call_input.done"
                | "response.output_item.done"
                | "response.completed"
                | "response.incomplete"
                | "response.failed"
                | "error"
        )
    )
}

impl EventMapper {
    pub fn new() -> Self {
        Self::default()
    }
    /// A mapper that keeps reasoning items for replay to `provider` and `model`.
    pub fn with_replay(provider: &str, model: &str) -> Self {
        Self {
            replay_owner: Some((provider.to_owned(), model.to_owned())),
            ..Self::default()
        }
    }
    /// Event texts and terminal rules of `dialect`; the default is `Codex`.
    pub fn with_dialect(mut self, dialect: Dialect) -> Self {
        self.dialect = dialect;
        self
    }
    pub fn terminal(&self) -> bool {
        self.terminal
    }
    pub fn committed(&self) -> bool {
        self.committed
    }

    pub fn event(&mut self, mut value: Value) -> Result<Vec<StreamChunk>, EventError> {
        if self.terminal {
            return Ok(Vec::new());
        }
        // Private replay state: no chunk and no commit, so it never closes the
        // safe-retry window; a discarded attempt drops its buffer with the mapper.
        if value["type"] == "response.output_item.done" && value["item"]["type"] == "reasoning" {
            self.whitespace = None;
            if self.replay_owner.is_some() {
                self.replay.push(replay_payload(value["item"].take()));
            }
            return Ok(Vec::new());
        }
        let mut chunks = Vec::new();
        match value["type"].as_str().unwrap_or("") {
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta" => {
                let delta = value["delta"].as_str().unwrap_or("");
                if !delta.is_empty() {
                    let thinking = matches!(
                        value["type"].as_str(),
                        Some(
                            "response.reasoning_summary_text.delta"
                                | "response.reasoning_text.delta"
                        )
                    );
                    chunks.push(if thinking {
                        StreamChunk::Thinking {
                            content: delta.to_owned(),
                            parent_tool_use_id: None,
                        }
                    } else {
                        StreamChunk::Text {
                            content: delta.to_owned(),
                            parent_tool_use_id: None,
                        }
                    });
                    self.committed = true;
                }
            }
            "response.output_item.added" => {
                self.whitespace = None;
                self.current = None;
                if value["item"]["type"] == "function_call" {
                    self.current = Some(self.open_tool(&value));
                }
            }
            "response.function_call_arguments.delta" => {
                let delta = value["delta"].as_str().unwrap_or("");
                // Observe even detached/unknown items: otherwise a degenerate
                // stream could keep idle timers alive indefinitely after done.
                // The whitespace-loop guard is Codex policy only.
                if self.dialect == Dialect::Codex {
                    self.observe_whitespace(&value, delta)?;
                }
                if let Some(index) = self.find_tool(&value) {
                    let tool = &mut self.tools[index];
                    if !tool.emitted {
                        tool.arguments.push_str(delta);
                    }
                }
            }
            "response.function_call_arguments.done" => {
                self.whitespace = None;
                if let Some(index) = self.find_tool(&value) {
                    let tool = &mut self.tools[index];
                    if !tool.emitted {
                        if let Some(arguments) = value["arguments"].as_str() {
                            tool.arguments = arguments.to_owned();
                        }
                        tool.arguments_done = true;
                    }
                }
                // Do not execute yet: output_item.done may supersede these args.
            }
            "response.output_item.done" => {
                self.whitespace = None;
                if value["item"]["type"] == "function_call" {
                    let index = self.open_tool(&value);
                    if !self.tools[index].emitted {
                        let item = &value["item"];
                        let tool = &mut self.tools[index];
                        if let Some(id) = item["call_id"].as_str() {
                            tool.call_id = id.to_owned();
                        }
                        if let Some(name) = item["name"].as_str() {
                            tool.name = name.to_owned();
                        }
                        if let Some(arguments) =
                            item["arguments"].as_str().filter(|s| !s.is_empty())
                        {
                            tool.arguments = arguments.to_owned();
                        }
                        self.emit_tool(index, &mut chunks);
                    }
                    if self.current == Some(index) {
                        self.current = None;
                    }
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                // omp maps these terminal statuses to an error stop, never a normal turn.
                if matches!(
                    value["response"]["status"].as_str(),
                    Some("failed" | "cancelled")
                ) {
                    return Err(failure(&value, self.dialect));
                }
                let response = &value["response"];
                let incomplete =
                    value["type"] == "response.incomplete" || response["status"] == "incomplete";
                let reason = response["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("");
                // A filtered turn is a failure, never a length stop to recover from.
                if self.dialect == Dialect::Generic && incomplete && reason == "content_filter" {
                    return Err(EventError {
                        message: "incomplete: content_filter".into(),
                        retryable: false,
                        whitespace_loop: false,
                    });
                }
                self.terminal = true;
                for index in 0..self.tools.len() {
                    if self.tools[index].arguments_done && !self.tools[index].emitted {
                        self.emit_tool(index, &mut chunks);
                    }
                }
                // Released only here, in the terminal batch. The one retry that can
                // follow a terminal event (the generic empty-completion retry) is
                // decided by the transport before this batch is forwarded and
                // discards it whole, so only the attempt that is kept persists.
                if let Some((provider, model)) = &self.replay_owner {
                    chunks.extend(self.replay.drain(..).map(|payload| {
                        StreamChunk::ProviderReplay {
                            item: ProviderReplayItem {
                                provider: provider.clone(),
                                model: model.clone(),
                                payload: std::sync::Arc::new(payload),
                            },
                        }
                    }));
                }
                if let Some(usage) = response.get("usage").filter(|usage| usage.is_object()) {
                    chunks.push(StreamChunk::Usage {
                        usage: parse_usage(usage),
                        session_id: None,
                    });
                }
                let completed =
                    !incomplete || (self.dialect == Dialect::Codex && reason == "steered");
                let stop_reason =
                    if self.tool_emitted && (completed || reason == "max_output_tokens") {
                        StopReason::ToolUse
                    } else if completed {
                        StopReason::EndTurn
                    } else {
                        StopReason::MaxTokens
                    };
                chunks.push(StreamChunk::TurnComplete { stop_reason });
            }
            "response.failed" | "error" => return Err(failure(&value, self.dialect)),
            // Includes response.metadata: profile captures its headers separately.
            _ => {}
        }
        Ok(chunks)
    }

    fn find_tool(&self, value: &Value) -> Option<usize> {
        if let Some(id) = value["item_id"].as_str().filter(|id| !id.is_empty()) {
            return self.by_id.get(id).copied();
        }
        if let Some(index) = integer(&value["output_index"]) {
            return self.by_index.get(&index).copied();
        }
        self.current
    }

    fn open_tool(&mut self, value: &Value) -> usize {
        let item = &value["item"];
        let item_id = item["id"].as_str().filter(|id| !id.is_empty());
        let output_index = integer(&value["output_index"]);
        let existing = item_id
            .and_then(|id| self.by_id.get(id).copied())
            .or_else(|| output_index.and_then(|index| self.by_index.get(&index).copied()))
            .or_else(|| {
                if item_id.is_none() && output_index.is_none() {
                    self.find_tool(value)
                } else {
                    None
                }
            });
        let index = existing.unwrap_or_else(|| {
            let index = self.tools.len();
            self.tools.push(Tool {
                item_id: item_id.map(str::to_owned),
                call_id: item["call_id"].as_str().unwrap_or("").to_owned(),
                name: item["name"].as_str().unwrap_or("").to_owned(),
                arguments: item["arguments"].as_str().unwrap_or("").to_owned(),
                ..Tool::default()
            });
            index
        });
        if let Some(id) = item_id {
            self.by_id.insert(id.to_owned(), index);
            self.tools[index].item_id = Some(id.to_owned());
        }
        if let Some(output_index) = output_index {
            self.by_index.insert(output_index, index);
        }
        index
    }

    fn emit_tool(&mut self, index: usize, chunks: &mut Vec<StreamChunk>) {
        let tool = &mut self.tools[index];
        tool.emitted = true;
        // Deduplicate only real ids: calls lacking one must not swallow each other.
        if !tool.call_id.is_empty() && !self.emitted_calls.insert(tool.call_id.clone()) {
            return;
        }
        chunks.push(StreamChunk::ToolUse {
            id: std::mem::take(&mut tool.call_id),
            name: std::mem::take(&mut tool.name),
            input: serde_json::from_str(&tool.arguments).unwrap_or_else(|_| json!({})),
        });
        self.tool_emitted = true;
        self.committed = true;
    }

    fn observe_whitespace(&mut self, value: &Value, delta: &str) -> Result<(), EventError> {
        if !delta
            .bytes()
            .all(|b| matches!(b, b'\t' | b'\n' | b'\r' | b' '))
        {
            self.whitespace = None;
            return Ok(());
        }
        let item_id = value["item_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .or_else(|| {
                self.current
                    .and_then(|index| self.tools[index].item_id.as_deref())
            })
            .unwrap_or("");
        let output_index = integer(&value["output_index"]);
        let sequence = integer(&value["sequence_number"]);
        if !self
            .whitespace
            .as_ref()
            .is_some_and(|run| run.item_id == item_id && run.output_index == output_index)
        {
            self.whitespace = Some(WhitespaceRun {
                item_id: item_id.to_owned(),
                output_index,
                events: 0,
                chars: 0,
                first_sequence: sequence,
                last_sequence: sequence,
            });
        }
        let run = self
            .whitespace
            .as_mut()
            .expect("initialized whitespace run");
        run.events += 1;
        run.chars += delta.len();
        run.last_sequence = sequence;
        if run.events < 256 && run.chars < 16 * 1024 {
            return Ok(());
        }
        let item_label = if run.item_id.is_empty() {
            String::new()
        } else {
            format!(" for item {}", run.item_id)
        };
        let sequence_label = match (run.first_sequence, run.last_sequence) {
            (Some(first), Some(last)) => format!(", sequence {first}..{last}"),
            _ => String::new(),
        };
        Err(EventError {
            message: format!(
                "Interrupted OpenAI Codex response after {} consecutive whitespace-only tool-call argument delta events ({} chars{sequence_label}){item_label}.",
                run.events, run.chars
            ),
            retryable: true,
            whitespace_loop: true,
        })
    }
}

/// omp's replay reduction of a reasoning item (`utils.ts`
/// `sanitizeOpenAIResponsesReasoningItemForReplay`): `id` is dropped, array
/// `summary`/`content` are kept, and `encrypted_content` is kept verbatim when it
/// is a string or `null`.
fn replay_payload(item: Value) -> Value {
    let mut reduced = serde_json::Map::with_capacity(4);
    reduced.insert("type".into(), Value::String("reasoning".into()));
    if let Value::Object(mut item) = item {
        for key in ["summary", "content"] {
            if let Some(value @ Value::Array(_)) = item.remove(key) {
                reduced.insert(key.into(), value);
            }
        }
        if let Some(value @ (Value::String(_) | Value::Null)) = item.remove("encrypted_content") {
            reduced.insert("encrypted_content".into(), value);
        }
    }
    Value::Object(reduced)
}

fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value
            .as_f64()
            .filter(|n| n.is_finite())
            .map(|n| n.trunc() as i64)
    })
}

fn tokens(value: &Value) -> Option<u32> {
    value.as_u64().map(|n| n.min(u64::from(u32::MAX)) as u32)
}

/// omp `populateResponsesUsageFromResponse`: when the reported total shows the
/// primary counts already include orchestration tokens, those are subtracted.
/// Without orchestration fields this is the plain mapping.
fn parse_usage(usage: &Value) -> UsageInfo {
    let details = &usage["input_tokens_details"];
    let output_details = &usage["output_tokens_details"];
    let input = tokens(&usage["input_tokens"]).unwrap_or(0);
    let output = tokens(&usage["output_tokens"]).unwrap_or(0);
    let orchestration_input = tokens(&details["orchestration_input_tokens"]).unwrap_or(0);
    let orchestration_output = tokens(&output_details["orchestration_output_tokens"]).unwrap_or(0);
    let orchestration_cached = tokens(&details["orchestration_input_cached_tokens"])
        .unwrap_or(0)
        .min(orchestration_input);
    let primary = i64::from(input) + i64::from(output);
    let separate = primary + i64::from(orchestration_input) + i64::from(orchestration_output);
    let included = usage["total_tokens"].as_f64().is_some_and(|total| {
        (orchestration_input > 0 || orchestration_output > 0)
            && (total - primary as f64).abs() <= (total - separate as f64).abs()
    });
    let (input, output, orchestration_cached) = if included {
        (
            input.saturating_sub(orchestration_input),
            output.saturating_sub(orchestration_output),
            orchestration_cached,
        )
    } else {
        (input, output, 0)
    };
    let cached = tokens(&details["cached_tokens"])
        .or_else(|| tokens(&usage["prompt_cache_hit_tokens"]))
        .map(|cached| cached.saturating_sub(orchestration_cached));
    let written = tokens(&details["cache_write_tokens"])
        .or_else(|| tokens(&usage["prompt_cache_miss_tokens"]));
    UsageInfo {
        input_tokens: input
            .saturating_sub(cached.unwrap_or(0))
            .saturating_sub(written.unwrap_or(0)),
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: written,
        reasoning_tokens: tokens(&output_details["reasoning_tokens"]),
    }
}

fn failure(value: &Value, dialect: Dialect) -> EventError {
    match dialect {
        Dialect::Codex => codex_failure(value),
        Dialect::Generic => {
            let message = generic_failure_text(value);
            EventError {
                retryable: generic_retryable(&message),
                message,
                whitespace_loop: false,
            }
        }
    }
}

/// omp `isProviderRetryableError` over a provider stream message: transient
/// transport/overload wording (`TRANSIENT_TRANSPORT_PATTERN`,
/// `PROVIDER_TRANSIENT_EXTRA_PATTERN`, unexpected socket close), never an
/// account usage limit.
fn generic_retryable(message: &str) -> bool {
    static TRANSIENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)\b(?:no[_ -]?capacity|(?:high|peak)[ _-]?demand|(?:at|over|insufficient)[ _-]?capacity|capacity[ _-]?(?:exceeded|exhausted)|peak[ _-]?load)\b",
            r"|overloaded|provider.?returned.?error|rate.?limit|too many requests|auth-gateway\s+5\d{2}(?:[:\s]|$)",
            r"|\b(?:429|500|502|503|504)\b|service.?unavailable|temporar(?:y|ily)[ _-]?unavailable|server.?error|internal.?error",
            r"|retry your request|network.?error|connection.?error|connection.?refused",
            r"|unable.?to.?connect\.\s*is the computer able to access the url\?|other side closed|use of closed network connection",
            r"|fetch failed|upstream.?connect|upstream.?request.?failed|reset before headers|socket hang up|timed? out|timeout",
            r"|terminated|retry delay|stream stall|no error details in response|HTTP2(?:StreamReset|RefusedStream|EnhanceYourCalm)",
            r"|nghttp2_(?:internal_error|refused_stream)|stream closed with error code nghttp2_(?:internal_error|refused_stream)",
            r"|\bE(?:TIMEOUT|NOTFOUND|NETUNREACH|HOSTUNREACH)\b|\bFailedToOpenSocket\b|no internet connection|couldn.?t reach",
            r"|couldn.?t connect to|tls certificate verification failed|malformed.?function.?call",
            r"|bad record mac|stream error.*received from peer|1302",
            r"|\b(?:the\s+)?socket connection (?:was )?closed unexpectedly\b|^\s*(?:error:\s*)?socket is closed\.?\s*$",
        ))
        .expect("fixed transient expression")
    });
    static USAGE_LIMIT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)usage.?limit|usage_limit_reached|usage_not_included|limit_reached|quota.?(?:exceeded|reached|insufficient)",
            r"|额度不足|额度耗尽|resource.?exhausted|exhausted your capacity|quota will reset|insufficient.?(?:balance|quota)",
            r"|balance.?exhausted|run out of credits|out of credits|out of (?:extra )?usage|spending[- _]?limit",
            r"|personal-team-blocked|clinepass limit|free limit reached on model|access_terminated_error",
        ))
        .expect("fixed usage-limit expression")
    });
    !USAGE_LIMIT.is_match(message) && TRANSIENT.is_match(message)
}

fn nullish(value: &Value) -> Option<&Value> {
    (!value.is_null()).then_some(value)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JavaScript template-literal text of a JSON value.
fn js_text(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::String(text) => text.clone(),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_text(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// omp generic texts: `error` → `Error Code <code>: <message>`; `response.failed`
/// and failed/cancelled terminals → `<code>: <message>`, `incomplete: <reason>`,
/// (terminals only) `status_details: <reason>`, else the unknown-error text.
fn generic_failure_text(value: &Value) -> String {
    if value["type"] == "error" {
        let error = nullish(&value["error"]).unwrap_or(value);
        let code = nullish(&error["code"]).map_or_else(|| "unknown".into(), js_text);
        let message = nullish(&error["message"]).map_or_else(|| "no message".into(), js_text);
        return format!("Error Code {code}: {message}");
    }
    let response = &value["response"];
    let error =
        nullish(&response["error"]).or_else(|| nullish(&response["status_details"]["error"]));
    if let Some(error) = error.filter(|error| truthy(error)) {
        let code = Some(&error["code"])
            .filter(|code| truthy(code))
            .map_or_else(|| "unknown".into(), js_text);
        let message = Some(&error["message"])
            .filter(|message| truthy(message))
            .map_or_else(|| "no message".into(), js_text);
        return format!("{code}: {message}");
    }
    let reason = &response["incomplete_details"]["reason"];
    if truthy(reason) {
        return format!("incomplete: {}", js_text(reason));
    }
    if value["type"] != "response.failed" {
        if let Some(reason) = response["status_details"]["reason"]
            .as_str()
            .filter(|reason| !reason.is_empty())
        {
            return format!("status_details: {reason}");
        }
    }
    "Unknown error (no error details in response)".into()
}

fn codex_failure(value: &Value) -> EventError {
    static RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i:processing your request|retry your request|temporar(?:y|ily)|overloaded|service.?unavailable|internal error|server error)|<StreamReset stream_id:\d+, error_code:(?:2|7), remote_reset:True>|peer closed connection without sending complete message body \(incomplete chunked read\)").expect("fixed retry expression")
    });
    let error = value
        .get("error")
        .filter(|error| error.is_object())
        .or_else(|| {
            value["response"]
                .get("error")
                .filter(|error| error.is_object())
        });
    let code = error
        .and_then(|error| error["code"].as_str().or_else(|| error["type"].as_str()))
        .or_else(|| value["code"].as_str())
        .unwrap_or("");
    let message = error
        .and_then(|error| error["message"].as_str())
        .or_else(|| value["message"].as_str())
        .or_else(|| value["response"]["message"].as_str())
        .unwrap_or("");
    let status = value["response"]["status"]
        .as_str()
        .or_else(|| value["status"].as_str())
        .unwrap_or("");
    let mut metadata = Vec::new();
    if !code.is_empty() {
        metadata.push(format!("code={code}"));
    }
    if !status.is_empty() {
        metadata.push(format!("status={status}"));
    }
    let label = if value["type"] == "error" {
        "Codex error event"
    } else {
        "Codex response failed"
    };
    let detail = if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    };
    let suffix = if metadata.is_empty() {
        String::new()
    } else {
        format!(" ({})", metadata.join(", "))
    };
    EventError {
        // Never echo the raw event: metadata/unknown fields can contain secrets.
        message: format!("{label}{detail}{suffix}"),
        retryable: matches!(
            code.to_ascii_lowercase().as_str(),
            "model_error"
                | "server_error"
                | "internal_error"
                | "unsupported_native_inflight_message"
        ) || RETRYABLE.is_match(message),
        whitespace_loop: false,
    }
}
