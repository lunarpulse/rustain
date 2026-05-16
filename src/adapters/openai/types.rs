//! OpenAI-compatible API wire-format types.

use serde::{Deserialize, Serialize};

use crate::domain::models::{CompletionOptions, Message, MessageRole, ToolDefinition};

// ─── Request Types ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OpenAiToolDef>,
}

#[derive(Debug, Serialize)]
pub struct OpenAiToolDef {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: OpenAiFunctionDef,
}

#[derive(Debug, Serialize)]
pub struct OpenAiFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl From<&ToolDefinition> for OpenAiToolDef {
    fn from(td: &ToolDefinition) -> Self {
        Self {
            tool_type: "function".to_string(),
            function: OpenAiFunctionDef {
                name: td.name.clone(),
                description: td.description.clone(),
                parameters: td.input_schema.clone(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OpenAiMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OpenAiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: OpenAiToolCallFunction,
}

#[derive(Debug, Serialize)]
pub struct OpenAiToolCallFunction {
    pub name: String,
    pub arguments: String,
}

impl From<(&[Message], &CompletionOptions)> for OpenAiRequest {
    fn from((messages, options): (&[Message], &CompletionOptions)) -> Self {
        let openai_messages: Vec<OpenAiMessage> = messages
            .iter()
            .filter_map(|msg| {
                let role = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
                };

                // Build content from text + images + tool results
                let mut content = msg.content.clone();

                // Append tool results
                for tr in &msg.tool_results {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&format!("Tool result ({}): {}", tr.tool_use_id, tr.content));
                }

                // Append image mentions as text (OpenAI-compatible endpoints handle
                // images differently than Anthropic; for now, mention them in text)
                for img in &msg.images {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&format!("[Image: {}]", img.media_type));
                }

                // Skip truly empty messages
                if content.is_empty() && msg.tool_uses.is_empty() {
                    return None;
                }

                let tool_calls = if msg.tool_uses.is_empty() {
                    None
                } else {
                    Some(
                        msg.tool_uses
                            .iter()
                            .map(|tu| OpenAiToolCall {
                                id: tu.id.clone(),
                                call_type: "function".to_string(),
                                function: OpenAiToolCallFunction {
                                    name: tu.name.clone(),
                                    arguments: tu.input.to_string(),
                                },
                            })
                            .collect(),
                    )
                };

                Some(OpenAiMessage {
                    role: role.to_string(),
                    content,
                    tool_calls,
                    tool_call_id: None,
                })
            })
            .collect();

        let tools: Vec<OpenAiToolDef> = options.tools.iter().map(OpenAiToolDef::from).collect();

        OpenAiRequest {
            model: options.model.clone(),
            messages: openai_messages,
            stream: true,
            temperature: options.temperature,
            tools,
        }
    }
}

// ─── SSE Response Event Types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OpenAiStreamEvent {
    #[allow(dead_code)]
    pub id: Option<String>,
    #[allow(dead_code)]
    pub object: String,
    #[allow(dead_code)]
    pub created: Option<u64>,
    #[allow(dead_code)]
    pub model: Option<String>,
    pub choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiChoice {
    pub index: usize,
    pub delta: OpenAiDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct OpenAiDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAiDeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiDeltaToolCall {
    pub index: usize,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    pub function: Option<OpenAiDeltaFunction>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiDeltaFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

// ─── Models List Response Types (Story 7.6 AC3) ────────────────────────────

/// Wire format for OpenAI-compatible `/v1/models` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsListResponse {
    pub data: Vec<ModelsListItem>,
}

/// Individual model entry from `/v1/models`.
/// Permissive — only `id` is required; other fields are provider-specific.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsListItem {
    pub id: String,
    /// Friendly display name (OpenRouter populates this; most providers do not).
    pub name: Option<String>,
    /// Context window length in tokens (OpenRouter).
    pub context_length: Option<u32>,
    /// Supported parameter names, e.g. `["tools", "top_p"]` (OpenRouter).
    pub supported_parameters: Option<Vec<String>>,
    /// Object type literal, e.g. `"model"` (OpenAI).
    pub object: Option<String>,
}
