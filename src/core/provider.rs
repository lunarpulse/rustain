use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use tokio::sync::mpsc;

use crate::types::stream::TuiStreamEvent;

/// Messages sent to the provider API
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: String,
    pub content: serde_json::Value,
}

/// Completion options
#[derive(Debug, Clone)]
pub struct CompletionOptions {
    pub model: String,
    pub max_tokens: u32,
    pub system_prompt: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub thinking_budget: Option<u32>,
}

impl Default for CompletionOptions {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 8192,
            system_prompt: None,
            temperature: None,
            top_p: None,
            thinking_budget: None,
        }
    }
}

/// Streaming completion result metadata
#[derive(Debug, Clone)]
pub struct StreamResult {
    pub stop_reason: String,
    pub tool_calls: Vec<ToolCallResult>,
}

#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Trait for streaming LLM providers.
#[async_trait]
pub trait StreamingProvider: Send + Sync {
    async fn stream_completion(
        &self,
        messages: &[Message],
        options: &CompletionOptions,
        tx: &mpsc::UnboundedSender<TuiStreamEvent>,
    ) -> Result<StreamResult>;
}

// ── SSE Parser State ────────────────────────────────────────────

#[derive(Debug)]
enum BlockState {
    Text,
    ToolUse {
        id: String,
        name: String,
        json_fragments: String,
    },
    Thinking,
}

// ── Anthropic Streaming Provider ────────────────────────────────

pub struct AnthropicStreamingProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicStreamingProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string()),
        }
    }

    fn build_request_body(
        &self,
        messages: &[Message],
        options: &CompletionOptions,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": options.model,
            "max_tokens": options.max_tokens,
            "messages": messages,
            "stream": true,
        });

        if let Some(ref system) = options.system_prompt {
            body["system"] = serde_json::json!(system);
        }
        if let Some(temp) = options.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(top_p) = options.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }

        body
    }
}

#[async_trait]
impl StreamingProvider for AnthropicStreamingProvider {
    async fn stream_completion(
        &self,
        messages: &[Message],
        options: &CompletionOptions,
        tx: &mpsc::UnboundedSender<TuiStreamEvent>,
    ) -> Result<StreamResult> {
        let body = self.build_request_body(messages, options);
        let url = format!("{}/v1/messages", self.base_url);

        let request = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(serde_json::to_string(&body)?);

        let mut es = EventSource::new(request)?;

        // Parser state
        let mut current_blocks: std::collections::HashMap<u32, BlockState> =
            std::collections::HashMap::new();
        let mut stop_reason = "end_turn".to_string();
        let mut tool_calls: Vec<ToolCallResult> = Vec::new();

        while let Some(event) = es.next().await {
            match event {
                Ok(Event::Open) => {
                    tracing::debug!("SSE connection opened");
                }
                Ok(Event::Message(msg)) => {
                    let data: serde_json::Value = match serde_json::from_str(&msg.data) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("Failed to parse SSE data: {}", e);
                            continue;
                        }
                    };

                    let event_type = data["type"].as_str().unwrap_or("");

                    match event_type {
                        "message_start" => {
                            // Extract usage from initial message
                            if let Some(usage) = data["message"]["usage"].as_object() {
                                let input_tokens =
                                    usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                                        as u32;
                                tx.send(TuiStreamEvent::Usage {
                                    input_tokens,
                                    output_tokens: 0,
                                    cache_creation_tokens: 0,
                                    cache_read_tokens: 0,
                                    context_window: 200_000,
                                })?;
                            }
                        }

                        "content_block_start" => {
                            let index = data["index"].as_u64().unwrap_or(0) as u32;
                            let block = &data["content_block"];
                            let block_type = block["type"].as_str().unwrap_or("");

                            match block_type {
                                "text" => {
                                    current_blocks.insert(index, BlockState::Text);
                                }
                                "tool_use" => {
                                    let id =
                                        block["id"].as_str().unwrap_or("").to_string();
                                    let name =
                                        block["name"].as_str().unwrap_or("").to_string();
                                    tx.send(TuiStreamEvent::ToolUse {
                                        id: id.clone(),
                                        name: name.clone(),
                                        input: serde_json::Value::Object(Default::default()),
                                        parent_tool_use_id: None,
                                    })?;
                                    current_blocks.insert(
                                        index,
                                        BlockState::ToolUse {
                                            id,
                                            name,
                                            json_fragments: String::new(),
                                        },
                                    );
                                }
                                "thinking" => {
                                    current_blocks.insert(index, BlockState::Thinking);
                                }
                                _ => {
                                    tracing::debug!(
                                        "Unknown content block type: {}",
                                        block_type
                                    );
                                }
                            }
                        }

                        "content_block_delta" => {
                            let index = data["index"].as_u64().unwrap_or(0) as u32;
                            let delta = &data["delta"];
                            let delta_type = delta["type"].as_str().unwrap_or("");

                            match delta_type {
                                "text_delta" => {
                                    if let Some(text) = delta["text"].as_str() {
                                        tx.send(TuiStreamEvent::Text {
                                            content: text.to_string(),
                                            parent_tool_use_id: None,
                                        })?;
                                    }
                                }
                                "thinking_delta" => {
                                    if let Some(thinking) = delta["thinking"].as_str() {
                                        tx.send(TuiStreamEvent::Thinking {
                                            content: thinking.to_string(),
                                            parent_tool_use_id: None,
                                        })?;
                                    }
                                }
                                "input_json_delta" => {
                                    // Accumulate partial JSON — don't parse until content_block_stop
                                    if let Some(partial) = delta["partial_json"].as_str() {
                                        if let Some(BlockState::ToolUse {
                                            json_fragments, ..
                                        }) = current_blocks.get_mut(&index)
                                        {
                                            json_fragments.push_str(partial);
                                        }
                                    }
                                }
                                _ => {
                                    tracing::debug!("Unknown delta type: {}", delta_type);
                                }
                            }
                        }

                        "content_block_stop" => {
                            let index = data["index"].as_u64().unwrap_or(0) as u32;
                            if let Some(block) = current_blocks.remove(&index) {
                                if let BlockState::ToolUse {
                                    id,
                                    name,
                                    json_fragments,
                                } = block
                                {
                                    let input: serde_json::Value =
                                        serde_json::from_str(&json_fragments)
                                            .unwrap_or(serde_json::Value::Object(Default::default()));
                                    tool_calls.push(ToolCallResult {
                                        id,
                                        name,
                                        input,
                                    });
                                }
                            }
                        }

                        "message_delta" => {
                            if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                                stop_reason = reason.to_string();
                            }
                            if let Some(usage) = data["usage"].as_object() {
                                let output_tokens = usage
                                    .get("output_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as u32;
                                tx.send(TuiStreamEvent::Usage {
                                    input_tokens: 0,
                                    output_tokens,
                                    cache_creation_tokens: 0,
                                    cache_read_tokens: 0,
                                    context_window: 200_000,
                                })?;
                            }
                        }

                        "message_stop" => {
                            break;
                        }

                        "error" => {
                            let error_msg = data["error"]["message"]
                                .as_str()
                                .unwrap_or("Unknown API error");
                            tx.send(TuiStreamEvent::Error {
                                content: error_msg.to_string(),
                            })?;
                            break;
                        }

                        "ping" => {}

                        _ => {
                            tracing::debug!("Unknown SSE event type: {}", event_type);
                        }
                    }
                }
                Err(e) => {
                    tx.send(TuiStreamEvent::Error {
                        content: format!("SSE error: {}", e),
                    })?;
                    break;
                }
            }
        }

        es.close();

        Ok(StreamResult {
            stop_reason,
            tool_calls,
        })
    }
}
