//! Ollama local LLM adapter implementing the `StreamingProvider` trait.
//!
//! Wire format: NDJSON (newline-delimited JSON), NOT SSE.
//! Ollama does not emit `data: ` frames — each line is a complete JSON object.
//! No API key required.
//!
//! # Troubleshooting
//!
//! If you see `404 model '...' not found — try `ollama pull ...``:
//! The model is not downloaded locally. Run `ollama pull <model>` first.

use std::fmt;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{ModelCapability, ModelDescriptor};
use crate::domain::models::{CompletionOptions, Message, MessageRole, StreamChunk, UsageInfo};
use crate::domain::ports::StreamingProvider;

/// Ollama API adapter. Implements `StreamingProvider` for local LLM streaming.
pub struct OllamaAdapter {
    client: reqwest::Client,
    model: String,
    base_url: String,
    abort_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    discovered_models: ArcSwap<Vec<ModelDescriptor>>,
}

impl OllamaAdapter {
    /// Create a new OllamaAdapter.
    ///
    /// # Arguments
    /// * `model` — Model identifier (e.g., `llama3.3:70b`)
    /// * `base_url` — Ollama API base URL (defaults to `http://localhost:11434`)
    pub fn new(model: String, base_url: Option<String>) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

        let resolved_base_url = crate::infrastructure::utils::normalize_base_url(
            &base_url.unwrap_or_else(|| "http://localhost:11434".to_string()),
        );

        Ok(Self {
            client,
            model,
            base_url: resolved_base_url,
            abort_handle: Arc::new(Mutex::new(None)),
            discovered_models: ArcSwap::from_pointee(Vec::new()),
        })
    }
}

impl OllamaAdapter {
    async fn fetch_model_capabilities(
        &self,
        model_name: &str,
    ) -> Option<(std::collections::HashSet<ModelCapability>, Option<u32>)> {
        let url = format!("{}/api/show", self.base_url);
        let request_body = OllamaShowRequest {
            model: model_name.to_string(),
        };

        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .timeout(std::time::Duration::from_secs(5))
            .json(&request_body)
            .send()
            .await
            .ok()?;

        if !response.status().is_success() {
            return None;
        }

        let show_resp: OllamaShowResponse = response.json().await.ok()?;
        let capabilities = show_resp.capabilities?;

        let mut caps = std::collections::HashSet::new();
        for cap in &capabilities {
            match cap.as_str() {
                "tools" => {
                    caps.insert(ModelCapability::ToolUse);
                }
                "vision" => {
                    caps.insert(ModelCapability::Vision);
                }
                "thinking" => {
                    caps.insert(ModelCapability::Thinking);
                }
                _ => {}
            }
        }

        let context_window = show_resp
            .model_info
            .iter()
            .find(|(k, _)| k.ends_with(".context_length"))
            .and_then(|(_, v)| v.as_u64())
            .map(|v| v as u32);

        Some((caps, context_window))
    }
}

impl fmt::Debug for OllamaAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OllamaAdapter")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[async_trait]
impl StreamingProvider for OllamaAdapter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, ProviderError> {
        if messages.is_empty() {
            return Err(ProviderError::Other(
                "Cannot send empty messages list to API".to_string(),
            ));
        }

        let request_body =
            OllamaChatRequest::from((messages.as_slice(), &options, self.model.as_str()));
        let url = format!("{}/api/chat", self.base_url);

        tracing::debug!(
            target_url = %url,
            model = %self.model,
            message_count = messages.len(),
            "Sending Ollama chat request"
        );

        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .timeout(std::time::Duration::from_secs(120))
            .json(&request_body)
            .send()
            .await
            .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;

        let status = response.status();
        if !status.is_success() {
            return match status.as_u16() {
                404 => Err(ProviderError::Other(format!(
                    "model '{}' not found — try `ollama pull {}`",
                    self.model, self.model
                ))),
                status_code if status_code >= 500 => {
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "unknown".to_string());
                    Err(ProviderError::ConnectionFailed(format!(
                        "Server error {}: {}",
                        status_code, body
                    )))
                }
                _ => {
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "unknown".to_string());
                    Err(ProviderError::Other(format!("HTTP {}: {}", status, body)))
                }
            };
        }

        let byte_stream = response.bytes_stream();
        let model = self.model.clone();

        let stream = futures::stream::unfold(
            (
                byte_stream,
                NdjsonLineBuffer::new(),
                OllamaStreamTransformer::new(model),
            ),
            |(mut byte_stream, mut ndjson_buf, mut transformer)| async move {
                loop {
                    // Drain pending chunks first (FIFO order)
                    if let Some(chunk) = transformer.pop_pending() {
                        return Some((chunk, (byte_stream, ndjson_buf, transformer)));
                    }

                    // Get next bytes from HTTP stream
                    match byte_stream.next().await {
                        Some(Ok(bytes)) => {
                            let lines = ndjson_buf.feed(&bytes);
                            for line in lines {
                                transformer.process_line(&line);
                            }
                            // Loop back to drain pending
                        }
                        Some(Err(e)) => {
                            return Some((
                                StreamChunk::Error {
                                    content: format!("Stream read error: {}", e),
                                },
                                (byte_stream, ndjson_buf, transformer),
                            ));
                        }
                        None => {
                            // Stream ended — emit any remaining TurnComplete
                            if let Some(chunk) = transformer.take_turn_complete() {
                                return Some((chunk, (byte_stream, ndjson_buf, transformer)));
                            }
                            return None;
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        let mut handle = self.abort_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        Ok(())
    }

    fn provider_id(&self) -> String {
        "ollama".to_string()
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        self.discovered_models.load().as_ref().clone()
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        let url = format!("{}/api/tags", self.base_url);
        let response = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => {
                let tags: OllamaTagsResponse = resp.json().await.map_err(|e| {
                    ProviderError::Other(format!("Failed to parse Ollama tags response: {}", e))
                })?;

                let futures: Vec<_> = tags
                    .models
                    .iter()
                    .enumerate()
                    .map(|(idx, m)| async move {
                        let result = self.fetch_model_capabilities(&m.name).await;
                        (idx, result)
                    })
                    .collect();
                let mut indexed_results: Vec<_> = futures::stream::iter(futures)
                    .buffer_unordered(8)
                    .collect()
                    .await;
                indexed_results.sort_by_key(|(idx, _)| *idx);
                let capabilities_results: Vec<_> =
                    indexed_results.into_iter().map(|(_, r)| r).collect();

                let models: Vec<ModelDescriptor> = tags
                    .models
                    .into_iter()
                    .zip(capabilities_results)
                    .map(|(m, caps_result)| {
                        let (capabilities, context_window) = match caps_result {
                            Some((caps, ctx)) => {
                                let ctx = ctx.unwrap_or_else(|| {
                                    guess_context_from_parameter_size(&m.details.parameter_size)
                                });
                                (caps, ctx)
                            }
                            None => {
                                tracing::debug!(
                                    "/api/show unavailable for '{}' — assuming ToolUse",
                                    m.name
                                );
                                let ctx =
                                    guess_context_from_parameter_size(&m.details.parameter_size);
                                let caps =
                                    std::collections::HashSet::from([ModelCapability::ToolUse]);
                                (caps, ctx)
                            }
                        };
                        ModelDescriptor {
                            model_id: m.name.clone(),
                            display_name: m.name,
                            provider_id: "ollama".to_string(),
                            context_window,
                            capabilities,
                            pricing_tier: Some("local".to_string()),
                            stale: false,
                        }
                    })
                    .collect();

                self.discovered_models.store(Arc::new(models));
                Ok(())
            }
            Ok(resp) => Err(ProviderError::Other(format!(
                "Health check failed: HTTP {}",
                resp.status()
            ))),
            Err(e) => Err(crate::adapters::provider::classify_reqwest_error(&e)),
        }
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
        use std::time::Instant;
        // Non-billable: GET /api/tags (Ollama's model list endpoint, free).
        let url = format!("{}/api/tags", self.base_url);
        let start = Instant::now();
        let response = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        let latency = start.elapsed();
        match response {
            Ok(resp) => {
                let status = resp.status().as_u16();
                match status {
                    200..=299 => Ok(crate::domain::ports::ProbeOutcome { latency }),
                    401 | 403 => Err(ProviderError::AuthenticationFailed),
                    404 | 405 => Err(ProviderError::EndpointUnsupported(status)),
                    _ => Err(ProviderError::Other(format!(
                        "Probe failed: HTTP {}",
                        status
                    ))),
                }
            }
            Err(e) => Err(crate::adapters::provider::classify_reqwest_error(&e)),
        }
    }
}

// ─── NDJSON Line Buffer ─────────────────────────────────────────────────────

/// Buffers raw bytes and emits complete NDJSON lines.
/// Unlike SSE, Ollama emits one JSON object per `\n`-delimited line.
struct NdjsonLineBuffer {
    line_buf: String,
}

impl NdjsonLineBuffer {
    fn new() -> Self {
        Self {
            line_buf: String::new(),
        }
    }

    /// Feed raw bytes into the buffer. Returns any complete lines.
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let text = String::from_utf8_lossy(bytes);

        for ch in text.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.line_buf);
                if !line.trim().is_empty() {
                    lines.push(line);
                }
            } else if ch != '\r' {
                self.line_buf.push(ch);
            }
        }

        lines
    }
}

// ─── Stream Transformer ─────────────────────────────────────────────────────

/// Converts NDJSON lines into domain `StreamChunk` values.
struct OllamaStreamTransformer {
    model: String,
    pending: std::collections::VecDeque<StreamChunk>,
    tool_call_counter: usize,
    turn_complete_emitted: bool,
}

impl OllamaStreamTransformer {
    fn new(model: String) -> Self {
        Self {
            model,
            pending: std::collections::VecDeque::new(),
            tool_call_counter: 0,
            turn_complete_emitted: false,
        }
    }

    fn pop_pending(&mut self) -> Option<StreamChunk> {
        self.pending.pop_front()
    }

    fn take_turn_complete(&mut self) -> Option<StreamChunk> {
        if self.turn_complete_emitted {
            return None;
        }
        self.turn_complete_emitted = true;
        Some(StreamChunk::TurnComplete {
            stop_reason: crate::domain::models::StopReason::EndTurn,
        })
    }

    fn process_line(&mut self, line: &str) {
        let response: OllamaChatResponse = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Failed to parse Ollama NDJSON line: {} — raw: {}", e, line);
                return;
            }
        };

        let message = response.message;

        // Text content
        if let Some(content) = message.as_ref().and_then(|m| m.content.clone()) {
            if !content.is_empty() {
                self.pending.push_back(StreamChunk::Text {
                    content,
                    parent_tool_use_id: None,
                });
            }
        }

        // Tool calls
        if let Some(tool_calls) = message.and_then(|m| m.tool_calls) {
            for tc in tool_calls {
                if let Some(function) = tc.function {
                    let input: serde_json::Value = serde_json::from_str(&function.arguments)
                        .unwrap_or_else(|e| {
                            tracing::warn!("Failed to parse Ollama tool arguments: {}", e);
                            serde_json::Value::Object(serde_json::Map::new())
                        });
                    self.tool_call_counter += 1;
                    self.pending.push_back(StreamChunk::ToolUse {
                        id: tc.id.unwrap_or_else(|| {
                            format!("ollama_tool_{}_{}", self.model, self.tool_call_counter)
                        }),
                        name: function.name,
                        input,
                    });
                }
            }
        }

        // Done — emit TurnComplete with usage
        if response.done {
            self.turn_complete_emitted = true;
            let stop_reason = match response.done_reason.as_deref().unwrap_or("stop") {
                "tool_calls" => crate::domain::models::StopReason::ToolUse,
                _ => crate::domain::models::StopReason::EndTurn,
            };
            self.pending
                .push_back(StreamChunk::TurnComplete { stop_reason });
            // Also emit usage if available
            if response.prompt_eval_count.is_some() || response.eval_count.is_some() {
                self.pending.push_back(StreamChunk::Usage {
                    usage: UsageInfo {
                        input_tokens: response.prompt_eval_count.unwrap_or(0),
                        output_tokens: response.eval_count.unwrap_or(0),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        reasoning_tokens: None,
                    },
                    session_id: None,
                });
            }
        }
    }
}

// ─── Wire-format Types ──────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OllamaToolDef>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaRequestToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaRequestToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OllamaRequestToolCallFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaRequestToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolDef {
    #[serde(rename = "type")]
    tool_type: String,
    function: OllamaToolFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    #[allow(dead_code)]
    model: String,
    #[serde(default)]
    message: Option<OllamaResponseMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OllamaResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCall {
    id: Option<String>,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    call_type: Option<String>,
    function: Option<OllamaToolCallFunction>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModelTag>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelTag {
    name: String,
    details: OllamaModelDetails,
}

#[derive(Debug, Deserialize)]
struct OllamaModelDetails {
    #[serde(default)]
    parameter_size: String,
}

#[derive(Debug, Serialize)]
struct OllamaShowRequest {
    model: String,
}

#[derive(Debug, Deserialize)]
struct OllamaShowResponse {
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    model_info: std::collections::HashMap<String, serde_json::Value>,
}

impl From<(&[Message], &CompletionOptions, &str)> for OllamaChatRequest {
    fn from((messages, options, model): (&[Message], &CompletionOptions, &str)) -> Self {
        let mut ollama_messages: Vec<OllamaMessage> = Vec::new();

        for msg in messages {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
            };

            // Build text content (text + images only; tool results are
            // emitted as separate role:"tool" messages per the OpenAI spec)
            let mut content = msg.content.clone();
            for img in &msg.images {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&format!("[Image: {}]", img.media_type));
            }

            let has_tool_results = !msg.tool_results.is_empty();
            let has_text = !content.is_empty();
            let has_tool_uses = !msg.tool_uses.is_empty();

            if has_tool_results {
                // Emit role:"tool" messages FIRST (before any role-carrying
                // text message) so they immediately follow the assistant
                // message with tool_calls. The OpenAI API requires that
                // every assistant message with tool_calls is IMMEDIATELY
                // followed by role:"tool" messages with matching tool_call_id.
                for tr in &msg.tool_results {
                    let tool_content = if tr.is_error {
                        format!("Error: {}", tr.content)
                    } else {
                        tr.content.clone()
                    };
                    ollama_messages.push(OllamaMessage {
                        role: "tool".to_string(),
                        content: tool_content,
                        tool_calls: None,
                        tool_call_id: Some(tr.tool_use_id.clone()),
                    });
                }

                // Then emit the role-carrying text message (if any text or tool_uses)
                if has_text || has_tool_uses {
                    let tool_calls = if has_tool_uses {
                        Some(
                            msg.tool_uses
                                .iter()
                                .map(|tu| OllamaRequestToolCall {
                                    id: tu.id.clone(),
                                    call_type: "function".to_string(),
                                    function: OllamaRequestToolCallFunction {
                                        name: tu.name.clone(),
                                        arguments: tu.input.to_string(),
                                    },
                                })
                                .collect(),
                        )
                    } else {
                        None
                    };
                    ollama_messages.push(OllamaMessage {
                        role: role.to_string(),
                        content,
                        tool_calls,
                        tool_call_id: None,
                    });
                }
            } else {
                let tool_calls = if has_tool_uses {
                    Some(
                        msg.tool_uses
                            .iter()
                            .map(|tu| OllamaRequestToolCall {
                                id: tu.id.clone(),
                                call_type: "function".to_string(),
                                function: OllamaRequestToolCallFunction {
                                    name: tu.name.clone(),
                                    arguments: tu.input.to_string(),
                                },
                            })
                            .collect(),
                    )
                } else {
                    None
                };
                ollama_messages.push(OllamaMessage {
                    role: role.to_string(),
                    content,
                    tool_calls,
                    tool_call_id: None,
                });
            }
        }

        let tools: Vec<OllamaToolDef> = options
            .tools
            .iter()
            .map(|td| OllamaToolDef {
                tool_type: "function".to_string(),
                function: OllamaToolFunction {
                    name: td.name.clone(),
                    description: td.description.clone(),
                    parameters: td.input_schema.clone(),
                },
            })
            .collect();

        OllamaChatRequest {
            model: model.to_string(),
            messages: ollama_messages,
            stream: true,
            tools,
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Guess context window from parameter size string (e.g. "7B", "70B").
fn guess_context_from_parameter_size(param_size: &str) -> u32 {
    let size_lower = param_size.to_lowercase();
    let numeric: String = size_lower
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let billions: u32 = numeric.parse().unwrap_or(0);
    match billions {
        0 => 8_192,
        1..=2 => 2_048,
        3..=5 => 4_096,
        6..=9 => 8_192,
        10..=15 => 16_384,
        16..=35 => 32_768,
        36..=69 => 32_768,
        _ => 32_768,
    }
}

// serde::Serialize is imported at the top of the module

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ollama_adapter_constructor_defaults_to_localhost() {
        let adapter = OllamaAdapter::new("llama3.3:70b".to_string(), None).unwrap();
        assert_eq!(adapter.provider_id(), "ollama");
        assert_eq!(adapter.model, "llama3.3:70b");
        let debug = format!("{:?}", adapter);
        assert!(debug.contains("localhost"));
    }

    #[test]
    fn test_ollama_adapter_debug_works() {
        let adapter = OllamaAdapter::new("llama3.3:70b".to_string(), None).unwrap();
        let debug = format!("{:?}", adapter);
        assert!(debug.contains("OllamaAdapter"));
        assert!(debug.contains("llama3.3:70b"));
    }

    #[test]
    fn test_ollama_adapter_provider_id() {
        let adapter = OllamaAdapter::new("llama3.3:70b".to_string(), None).unwrap();
        assert_eq!(adapter.provider_id(), "ollama");
    }

    #[test]
    fn test_parameter_size_to_context_window() {
        assert_eq!(guess_context_from_parameter_size("7B"), 8_192);
        assert_eq!(guess_context_from_parameter_size("8B"), 8_192);
        assert_eq!(guess_context_from_parameter_size("13B"), 16_384);
        assert_eq!(guess_context_from_parameter_size("70B"), 32_768);
        assert_eq!(guess_context_from_parameter_size("3B"), 4_096);
        assert_eq!(guess_context_from_parameter_size("1B"), 2_048);
        assert_eq!(guess_context_from_parameter_size("unknown"), 8_192);
        assert_eq!(guess_context_from_parameter_size(""), 8_192);
        assert_eq!(guess_context_from_parameter_size("117B"), 32_768);
        assert_eq!(guess_context_from_parameter_size("47B"), 32_768);
        assert_eq!(guess_context_from_parameter_size("14B"), 16_384);
        assert_eq!(guess_context_from_parameter_size("0.5B"), 8_192);
    }

    #[test]
    fn test_ollama_adapter_list_models_empty_before_health_check() {
        let adapter = OllamaAdapter::new("llama3.3:70b".to_string(), None).unwrap();
        assert!(adapter.list_models().is_empty());
    }

    #[test]
    fn test_ndjson_line_buffer_basic() {
        let mut buf = NdjsonLineBuffer::new();
        let lines = buf.feed(b"{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "{\"a\":1}");
        assert_eq!(lines[1], "{\"b\":2}");
    }

    #[test]
    fn test_ndjson_line_buffer_partial_line() {
        let mut buf = NdjsonLineBuffer::new();
        let l1 = buf.feed(b"{\"a\":1}");
        assert!(l1.is_empty());
        let l2 = buf.feed(b"\n{\"b\":2}\n");
        assert_eq!(l2.len(), 2);
    }

    #[test]
    fn test_ollama_stream_transformer_text() {
        let mut t = OllamaStreamTransformer::new("llama3".to_string());
        t.process_line(
            r#"{"model":"llama3","message":{"role":"assistant","content":"Hello"},"done":false}"#,
        );
        let chunk = t.pop_pending().unwrap();
        match chunk {
            StreamChunk::Text { content, .. } => assert_eq!(content, "Hello"),
            _ => panic!("Expected Text chunk"),
        }
    }

    #[test]
    fn test_ollama_stream_transformer_done() {
        let mut t = OllamaStreamTransformer::new("llama3".to_string());
        t.process_line(r#"{"model":"llama3","done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":2}"#);
        let chunk = t.pop_pending().unwrap();
        match chunk {
            StreamChunk::TurnComplete { .. } => {}
            _ => panic!("Expected TurnComplete chunk"),
        }
        let chunk2 = t.pop_pending().unwrap();
        match chunk2 {
            StreamChunk::Usage { usage, .. } => {
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 2);
            }
            _ => panic!("Expected Usage chunk"),
        }
    }
}
