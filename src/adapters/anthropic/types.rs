//! Anthropic API wire-format types (request and response).
//! These types live exclusively in the adapter layer — domain never sees them.

use serde::{Deserialize, Serialize};

use crate::domain::models::{CompletionOptions, Message, MessageRole, ToolDefinition};

// ─── Request Types ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AnthropicMetadata>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl From<&ToolDefinition> for AnthropicToolDef {
    fn from(td: &ToolDefinition) -> Self {
        Self {
            name: td.name.clone(),
            description: td.description.clone(),
            input_schema: td.input_schema.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<AnthropicContent>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum AnthropicContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

#[derive(Debug, Serialize)]
pub struct AnthropicMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

impl From<(&[Message], &CompletionOptions)> for AnthropicRequest {
    fn from((messages, options): (&[Message], &CompletionOptions)) -> Self {
        let system = if options.system_prompt.is_empty() {
            None
        } else {
            Some(options.system_prompt.clone())
        };

        let anthropic_messages: Vec<AnthropicMessage> = messages
            .iter()
            .filter_map(|msg| {
                let role = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                };

                let mut content = Vec::new();

                // Add tool results first (they're part of a user message in Anthropic's format)
                for tr in &msg.tool_results {
                    content.push(AnthropicContent::ToolResult {
                        tool_use_id: tr.tool_use_id.clone(),
                        content: tr.content.clone(),
                        is_error: tr.is_error,
                    });
                }

                // Add text content (skip empty to avoid API rejection)
                // Prepend context_prefix if present (used for session expiry rebuild)
                let text_content = match &msg.context_prefix {
                    Some(prefix) if !prefix.is_empty() => {
                        format!("{}\n\n{}", prefix, msg.content)
                    }
                    _ => msg.content.clone(),
                };
                if !text_content.is_empty() {
                    content.push(AnthropicContent::Text {
                        text: text_content,
                    });
                }

                // Add tool_use blocks (assistant messages in multi-turn tool conversations)
                for tu in &msg.tool_uses {
                    content.push(AnthropicContent::ToolUse {
                        id: tu.id.clone(),
                        name: tu.name.clone(),
                        input: tu.input.clone(),
                    });
                }

                // Safety: skip messages with truly empty content (programming error).
                // The Anthropic API rejects empty text blocks, so never synthesize one.
                if content.is_empty() {
                    tracing::warn!(
                        "Skipping message with empty content (role={:?}) — this indicates a bug in message construction",
                        msg.role
                    );
                    return None;
                }

                Some(AnthropicMessage {
                    role: role.to_string(),
                    content,
                })
            })
            .collect();

        let tools: Vec<AnthropicToolDef> =
            options.tools.iter().map(AnthropicToolDef::from).collect();

        AnthropicRequest {
            model: options.model.clone(),
            max_tokens: options.max_tokens,
            system,
            messages: anthropic_messages,
            stream: true,
            temperature: options.temperature,
            tools,
            metadata: None,
        }
    }
}

// ─── SSE Response Event Types ───────────────────────────────────────────────

/// Top-level SSE event parsed from the `data:` field.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageStartPayload },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockInfo,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: DeltaType },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: usize },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDeltaPayload,
        usage: Option<OutputUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop {},
    #[serde(rename = "ping")]
    Ping {},
    #[serde(rename = "error")]
    Error { error: ErrorPayload },
}

#[derive(Debug, Deserialize)]
pub struct MessageStartPayload {
    pub usage: Option<InputUsage>,
}

#[derive(Debug, Deserialize)]
pub struct InputUsage {
    pub input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct OutputUsage {
    pub output_tokens: u32,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)] // Fields used via serde deserialization + pattern matching
pub enum ContentBlockInfo {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[allow(clippy::enum_variant_names)] // Variants mirror Anthropic API naming
#[allow(dead_code)] // Fields used via serde deserialization + pattern matching
pub enum DeltaType {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
}

#[derive(Debug, Deserialize)]
pub struct MessageDeltaPayload {
    #[serde(default)]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ErrorPayload {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::ToolResultMessage;

    #[test]
    fn test_anthropic_request_from_messages_basic() {
        let messages = vec![Message {
            role: MessageRole::User,
            content: "hello".into(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
        }];
        let options = CompletionOptions {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 8192,
            system_prompt: "You are helpful.".into(),
            temperature: None,
            tools: vec![],
        };

        let req = AnthropicRequest::from((messages.as_slice(), &options));

        assert_eq!(req.model, "claude-sonnet-4-6");
        assert_eq!(req.max_tokens, 8192);
        assert_eq!(req.system.as_deref(), Some("You are helpful."));
        assert!(req.stream);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["messages"][0]["content"][0]["type"], "text");
        assert_eq!(json["messages"][0]["content"][0]["text"], "hello");
        assert!(json.get("temperature").is_none());
    }

    #[test]
    fn test_anthropic_request_empty_system_prompt_omitted() {
        let messages = vec![Message {
            role: MessageRole::User,
            content: "hi".into(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
        }];
        let options = CompletionOptions {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 1024,
            system_prompt: String::new(),
            temperature: Some(0.7),
            tools: vec![],
        };

        let req = AnthropicRequest::from((messages.as_slice(), &options));
        assert!(req.system.is_none());
        assert_eq!(req.temperature, Some(0.7));

        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("system").is_none());
        let temp = json["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 0.01);
    }

    #[test]
    fn test_anthropic_request_with_tool_results() {
        let messages = vec![Message {
            role: MessageRole::User,
            content: String::new(),
            images: vec![],
            tool_results: vec![ToolResultMessage {
                tool_use_id: "tool_abc".into(),
                content: "file contents here".into(),
                is_error: false,
            }],
            tool_uses: vec![],
            context_prefix: None,
        }];
        let options = CompletionOptions {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 8192,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
        };

        let req = AnthropicRequest::from((messages.as_slice(), &options));
        let json = serde_json::to_value(&req).unwrap();

        assert_eq!(json["messages"][0]["content"][0]["type"], "tool_result");
        assert_eq!(json["messages"][0]["content"][0]["tool_use_id"], "tool_abc");
    }

    #[test]
    fn test_anthropic_event_deserialization_text_delta() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let event: AnthropicEvent = serde_json::from_str(json).unwrap();
        match event {
            AnthropicEvent::ContentBlockDelta {
                index,
                delta: DeltaType::TextDelta { text },
            } => {
                assert_eq!(index, 0);
                assert_eq!(text, "Hello");
            }
            _ => panic!("Expected ContentBlockDelta with TextDelta"),
        }
    }

    #[test]
    fn test_anthropic_event_deserialization_message_delta() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;
        let event: AnthropicEvent = serde_json::from_str(json).unwrap();
        match event {
            AnthropicEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(usage.unwrap().output_tokens, 42);
            }
            _ => panic!("Expected MessageDelta"),
        }
    }

    #[test]
    fn test_anthropic_event_deserialization_error() {
        let json = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let event: AnthropicEvent = serde_json::from_str(json).unwrap();
        match event {
            AnthropicEvent::Error { error } => {
                assert_eq!(error.error_type, "overloaded_error");
                assert_eq!(error.message, "Overloaded");
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_empty_content_message_is_filtered_out() {
        // Regression test: empty content messages must not produce empty text blocks
        // (Anthropic API rejects "text content blocks must be non-empty")
        let messages = vec![
            Message {
                role: MessageRole::User,
                content: "hello".into(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(), // Empty content, no tool_uses
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
            },
        ];
        let options = CompletionOptions {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 8192,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
        };

        let req = AnthropicRequest::from((messages.as_slice(), &options));

        // Empty assistant message should be filtered out
        assert_eq!(req.messages.len(), 1, "Empty message should be filtered out");
        assert_eq!(req.messages[0].role, "user");

        // Verify no empty text blocks exist anywhere in the request
        let json = serde_json::to_value(&req).unwrap();
        for msg in json["messages"].as_array().unwrap() {
            for block in msg["content"].as_array().unwrap() {
                if block["type"] == "text" {
                    assert!(
                        !block["text"].as_str().unwrap().is_empty(),
                        "Found empty text block — this would cause HTTP 400"
                    );
                }
            }
        }
    }

    #[test]
    fn test_assistant_tool_use_blocks_serialized() {
        // Regression test: assistant messages with tool_use blocks must include them
        // for the Anthropic API to match tool_results in the next user message
        use crate::domain::models::ToolUseMessage;

        let messages = vec![
            Message {
                role: MessageRole::User,
                content: "read file.txt".into(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "I'll read that file.".into(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![ToolUseMessage {
                    id: "toolu_123".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "file.txt"}),
                }],
                context_prefix: None,
            },
            Message {
                role: MessageRole::User,
                content: String::new(),
                images: vec![],
                tool_results: vec![ToolResultMessage {
                    tool_use_id: "toolu_123".into(),
                    content: "file contents here".into(),
                    is_error: false,
                }],
                tool_uses: vec![],
                context_prefix: None,
            },
        ];
        let options = CompletionOptions {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 8192,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
        };

        let req = AnthropicRequest::from((messages.as_slice(), &options));
        let json = serde_json::to_value(&req).unwrap();

        // Assistant message should have both text and tool_use blocks
        let assistant_content = &json["messages"][1]["content"];
        assert_eq!(assistant_content[0]["type"], "text");
        assert_eq!(assistant_content[0]["text"], "I'll read that file.");
        assert_eq!(assistant_content[1]["type"], "tool_use");
        assert_eq!(assistant_content[1]["id"], "toolu_123");
        assert_eq!(assistant_content[1]["name"], "Read");

        // Tool result user message should have tool_result block
        let tool_result_content = &json["messages"][2]["content"];
        assert_eq!(tool_result_content[0]["type"], "tool_result");
        assert_eq!(tool_result_content[0]["tool_use_id"], "toolu_123");
    }
}
