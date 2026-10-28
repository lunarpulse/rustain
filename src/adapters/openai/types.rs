//! OpenAI-compatible API wire-format types.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::profile::{ChatCompat, ChatImages};
use crate::domain::models::{CompletionOptions, Message, MessageRole, ToolDefinition};

/// omp `NON_VISION_IMAGE_PLACEHOLDER`, sent once per message in place of its images.
const NON_VISION_IMAGE_PLACEHOLDER: &str = "[image omitted: model does not support vision]";

// ─── Request Types ──────────────────────────────────────────────────────────

/// Declaration order is wire order.
#[derive(Debug, Serialize)]
pub struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OpenAiToolDef>,
    /// Request token-usage statistics in the stream. Per the OpenAI streaming
    /// spec, usage data is NOT sent unless `stream_options.include_usage` is
    /// `true`. Providers that don't recognize the key ignore it (JSON tolerant
    /// deserialization) — worst case is `None` usage, same as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
}

/// `stream_options` payload — currently only `include_usage` is defined by the
/// OpenAI spec. Sent as `{"include_usage": true}` when streaming.
#[derive(Debug, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
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
    pub content: OpenAiContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// DeepSeek v4 thinking mode: echoed back verbatim on assistant messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

/// A message's content: plain text, or typed parts when images are encoded.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum OpenAiContent {
    Text(String),
    Parts(Vec<Value>),
}

impl OpenAiContent {
    /// The plain text; empty for typed parts.
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Parts(_) => "",
        }
    }
    fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) => text.is_empty(),
            Self::Parts(parts) => parts.is_empty(),
        }
    }
}

impl PartialEq<&str> for OpenAiContent {
    fn eq(&self, other: &&str) -> bool {
        matches!(self, Self::Text(text) if text == other)
    }
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
        Self::new(messages, options, &ChatCompat::default())
    }
}

/// One message's text and images under the profile's image encoding.
fn content(msg: &Message, images: ChatImages) -> OpenAiContent {
    let text = match &msg.context_prefix {
        Some(prefix) if !prefix.is_empty() => {
            let mut content = String::with_capacity(prefix.len() + 2 + msg.content.len());
            content.push_str(prefix);
            content.push_str("\n\n");
            content.push_str(&msg.content);
            content
        }
        _ => msg.content.clone(),
    };
    if msg.images.is_empty() {
        return OpenAiContent::Text(text);
    }
    match images {
        ChatImages::Legacy => {
            let mut text = text;
            for img in &msg.images {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!("[Image: {}]", img.media_type));
            }
            OpenAiContent::Text(text)
        }
        ChatImages::Parts => {
            let mut parts = Vec::with_capacity(1 + msg.images.len());
            if !text.is_empty() {
                parts.push(json!({"type":"text","text":text}));
            }
            for img in &msg.images {
                parts.push(json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}", img.media_type, img.data)}}));
            }
            OpenAiContent::Parts(parts)
        }
        ChatImages::Omitted => {
            let mut parts = Vec::with_capacity(2);
            if !text.is_empty() {
                parts.push(json!({"type":"text","text":text}));
            }
            parts.push(json!({"type":"text","text":NON_VISION_IMAGE_PLACEHOLDER}));
            OpenAiContent::Parts(parts)
        }
    }
}

impl OpenAiRequest {
    pub fn new(messages: &[Message], options: &CompletionOptions, compat: &ChatCompat) -> Self {
        let mut openai_messages: Vec<OpenAiMessage> = Vec::new();
        // The turn's system prompt leads the conversation.
        if !options.system_prompt.trim().is_empty() {
            openai_messages.push(OpenAiMessage {
                role: "system".to_string(),
                content: OpenAiContent::Text(options.system_prompt.clone()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }

        for msg in messages {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
            };

            // Build text content (text + images only; tool results are
            // emitted as separate role:"tool" messages per the OpenAI spec).
            // Typed image parts are a user-message encoding.
            let images = if msg.role == MessageRole::User {
                compat.images
            } else {
                ChatImages::Legacy
            };
            let content = content(msg, images);

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
                        content: OpenAiContent::Text(tool_content),
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
            store: compat.store_false.then_some(false),
            temperature: options.temperature,
            tools,
            // Always ask for usage stats when streaming. Providers that don't
            // support `stream_options` ignore the key (no error); supported
            // providers (OpenAI, DeepSeek, OpenRouter) return usage on the
            // final SSE chunk. See case file: openai-usage-calculation.
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            max_completion_tokens: compat.max_completion_tokens,
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
    /// Token usage. Only present on the final SSE chunk when the request set
    /// `stream_options.include_usage = true`. `#[serde(default)]` ensures every
    /// non-final chunk (which omits the key) deserializes as `None`.
    #[serde(default)]
    pub usage: Option<OpenAiUsage>,
}

/// Token usage payload sent by OpenAI-compatible providers on the final stream
/// chunk. All sub-fields are permissive (`#[serde(default)]`) for cross-provider
/// compatibility (DeepSeek, Gemini compat shim, OpenRouter passthrough).
#[derive(Debug, Deserialize, Default)]
pub struct OpenAiUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Redundant sum (prompt + completion). Not all providers populate it
    /// correctly — never read; totals are derived in the ledger/cost layer.
    #[serde(default)]
    pub total_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<OpenAiCompletionTokensDetails>,
}

/// Breakdown of prompt-side token usage (e.g. cached tokens). Currently unused
/// by Rustain's cost model (designed around Anthropic's cache semantics); kept
/// for forward-compat so unknown sub-keys don't break deserialization.
#[derive(Debug, Deserialize, Default)]
pub struct OpenAiPromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

/// Breakdown of completion-side token usage. `reasoning_tokens` carries the
/// o1/o3/DeepSeek-R1 reasoning effort and is mapped into `UsageInfo`.
#[derive(Debug, Deserialize, Default)]
pub struct OpenAiCompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::ToolResultMessage;

    #[test]
    fn story19_19_review_context_prefix_reaches_openai_user_message_and_preserves_tool_order() {
        let messages = [
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![crate::domain::models::ToolUseMessage {
                    id: "call-1".into(),
                    name: "lookup".into(),
                    input: serde_json::json!({}),
                }],
                context_prefix: None,
                reasoning_content: None,
                provider_replay: vec![],
            },
            Message {
                role: MessageRole::User,
                content: "ordinary prompt".into(),
                images: vec![],
                tool_results: vec![ToolResultMessage {
                    tool_use_id: "call-1".into(),
                    content: "result".into(),
                    is_error: false,
                }],
                tool_uses: vec![],
                context_prefix: Some("[selected-recipient-item]".into()),
                reasoning_content: None,
                provider_replay: vec![],
            },
        ];
        let options = CompletionOptions {
            session_id: None,
            model: "model".into(),
            max_tokens: 64,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
        };
        let json = serde_json::to_value(OpenAiRequest::from((&messages[..], &options))).unwrap();
        let wire_messages = json["messages"].as_array().unwrap();
        assert_eq!(wire_messages.len(), 3);
        assert_eq!(wire_messages[0]["role"], "assistant");
        assert_eq!(wire_messages[1]["role"], "tool");
        assert_eq!(wire_messages[2]["role"], "user");
        let text = wire_messages[2]["content"].as_str().unwrap();
        assert!(text.contains("[selected-recipient-item]"));
        assert!(text.ends_with("\n\nordinary prompt"));
    }
}
