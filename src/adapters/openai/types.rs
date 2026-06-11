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
    /// DeepSeek v4 thinking mode: echoed back verbatim on assistant messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
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
        let mut openai_messages: Vec<OpenAiMessage> = Vec::new();

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
                    openai_messages.push(OpenAiMessage {
                        role: "tool".to_string(),
                        content: tool_content,
                        tool_calls: None,
                        tool_call_id: Some(tr.tool_use_id.clone()),
                        reasoning_content: None,
                    });
                }

                // Then emit the role-carrying text message (if any text or tool_uses)
                if has_text || has_tool_uses {
                    let tool_calls = if has_tool_uses {
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
                    } else {
                        None
                    };
                    openai_messages.push(OpenAiMessage {
                        role: role.to_string(),
                        content,
                        tool_calls,
                        tool_call_id: None,
                        reasoning_content: msg.reasoning_content.clone(),
                    });
                }
            } else {
                // No tool results: single message as before
                if content.is_empty() && !has_tool_uses {
                    continue;
                }
                let tool_calls = if has_tool_uses {
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
                } else {
                    None
                };
                openai_messages.push(OpenAiMessage {
                    role: role.to_string(),
                    content,
                    tool_calls,
                    tool_call_id: None,
                    reasoning_content: msg.reasoning_content.clone(),
                });
            }
        }

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
    /// DeepSeek v4 thinking mode: reasoning/thinking content in deltas.
    #[serde(default)]
    pub reasoning_content: Option<String>,
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
