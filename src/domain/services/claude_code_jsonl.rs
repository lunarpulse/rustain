//! Pure JSONL parser for Claude Code session files.
//!
//! No I/O here — all functions take `&str` and return domain types.
//! Used by `ClaudeCodeImporter` (adapters/importers/claude_code.rs).

use std::path::Path;

use serde::{Deserialize, Deserializer};

use crate::domain::models::conversation::{ChatMessage, generate_message_id};
use crate::domain::models::session_meta::extract_title_from_first_message;
use crate::domain::models::{ContentBlockType, MessageRole};
use crate::domain::models::{ToolCallInfo, ToolResultInfo};
use crate::domain::services::import::ImportCandidate;

/// Parse error for JSONL lines.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

// ── Wire types for Claude Code JSONL ─────────────────────────────────────────

/// A single top-level entry in a Claude Code `.jsonl` file.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeCodeLine {
    /// The `type` field — "user", "assistant", "system", "file-history-snapshot",
    /// "permission-mode", or any future value.
    #[serde(rename = "type")]
    pub line_type: String,
    /// Message UUID — stable across re-imports.
    pub uuid: Option<String>,
    /// ISO8601 timestamp string.
    pub timestamp: Option<String>,
    /// `true` for injected metadata lines that are NOT real user turns.
    #[serde(rename = "isMeta", default)]
    pub is_meta: bool,
    /// The inner message payload (present on `user` and `assistant` lines).
    pub message: Option<ClaudeCodeMessage>,
}

/// The `message` object inside a `ClaudeCodeLine`.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeCodeMessage {
    #[allow(dead_code)]
    pub role: Option<String>,
    pub content: Option<ClaudeCodeContent>,
}

/// Polymorphic `message.content` — either a plain string or a list of blocks.
#[derive(Debug, Clone)]
pub enum ClaudeCodeContent {
    Text(String),
    Blocks(Vec<ClaudeCodeBlock>),
}

impl<'de> Deserialize<'de> for ClaudeCodeContent {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(d)?;
        match value {
            serde_json::Value::String(s) => Ok(ClaudeCodeContent::Text(s)),
            serde_json::Value::Array(_) => {
                let blocks: Vec<ClaudeCodeBlock> =
                    serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(ClaudeCodeContent::Blocks(blocks))
            }
            other => {
                // Unexpected shape — treat as text representation
                Ok(ClaudeCodeContent::Text(other.to_string()))
            }
        }
    }
}

/// A single content block inside an assistant or user message.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeCodeBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: serde_json::Value,
        is_error: Option<bool>,
    },
    Image,
    #[serde(other)]
    Unknown,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Parse a single JSONL line.
///
/// Returns `Ok(None)` for blank lines, `Ok(Some(_))` for valid entries,
/// `Err(_)` for malformed JSON.
pub fn parse_jsonl_line(line: &str) -> Result<Option<ClaudeCodeLine>, ImportError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let parsed: ClaudeCodeLine = serde_json::from_str(trimmed)?;
    Ok(Some(parsed))
}

/// Convert a slice of parsed lines into a `Vec<ChatMessage>`.
///
/// Implements the Claude Code → rustain mapping table (AC10):
/// - Skips `type ∈ {"system", "file-history-snapshot", "permission-mode"}` and
///   `isMeta == true` lines.
/// - Merges `user(tool_result)` blocks onto the previous assistant message's
///   `tool_calls[*].result` by `tool_use_id`.
/// - Builds one `ChatMessage{Assistant}` per assistant line.
pub fn convert_lines_to_chat_messages(lines: &[ClaudeCodeLine]) -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = Vec::new();

    for line in lines {
        // Skip non-conversation line types
        match line.line_type.as_str() {
            "system" | "file-history-snapshot" | "permission-mode" => continue,
            _ => {}
        }
        // Skip injected metadata
        if line.is_meta {
            continue;
        }

        let Some(ref msg) = line.message else {
            continue;
        };
        let Some(ref content) = msg.content else {
            continue;
        };

        match line.line_type.as_str() {
            "user" => {
                match content {
                    ClaudeCodeContent::Text(text) => {
                        let created_at = parse_timestamp(line.timestamp.as_deref());
                        let id = line.uuid.clone().unwrap_or_else(generate_message_id);
                        messages.push(ChatMessage {
                            id,
                            role: MessageRole::User,
                            content: text.clone(),
                            content_blocks: vec![ContentBlockType::Text],
                            tool_calls: vec![],
                            created_at,
                            token_count: None,
                            stop_reason: None,
                            synthetic: false,
                            images: vec![],
                            origin: crate::domain::models::ChannelKind::Terminal,
                            authorship: Default::default(),
                            retracted_at_ms: None,
                        });
                    }
                    ClaudeCodeContent::Blocks(blocks) => {
                        // A user turn with blocks is a tool_result wrapper —
                        // merge each tool_result onto the previous assistant message.
                        // DF-131: warn for image blocks in user turns (not imported).
                        let image_count = blocks
                            .iter()
                            .filter(|b| matches!(b, ClaudeCodeBlock::Image))
                            .count();
                        if image_count > 0 {
                            tracing::warn!(
                                image_count,
                                "Skipping {} image block(s) in user turn — inline images not supported by importer",
                                image_count
                            );
                        }

                        let tool_results: Vec<&ClaudeCodeBlock> = blocks
                            .iter()
                            .filter(|b| matches!(b, ClaudeCodeBlock::ToolResult { .. }))
                            .collect();

                        if tool_results.is_empty() {
                            // Non-tool-result blocks in a user turn — treat as
                            // text if there are text blocks, otherwise skip.
                            let text_parts: Vec<String> = blocks
                                .iter()
                                .filter_map(|b| {
                                    if let ClaudeCodeBlock::Text { text } = b {
                                        Some(text.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            if !text_parts.is_empty() {
                                let created_at = parse_timestamp(line.timestamp.as_deref());
                                let id = line.uuid.clone().unwrap_or_else(generate_message_id);
                                messages.push(ChatMessage {
                                    id,
                                    role: MessageRole::User,
                                    content: text_parts.join("\n\n"),
                                    content_blocks: vec![ContentBlockType::Text],
                                    tool_calls: vec![],
                                    created_at,
                                    token_count: None,
                                    stop_reason: None,
                                    synthetic: false,
                                    images: vec![],
                                    origin: crate::domain::models::ChannelKind::Terminal,
                                    authorship: Default::default(),
                                    retracted_at_ms: None,
                                });
                            }
                            continue;
                        }

                        // Find the most recent assistant message to attach results to
                        let last_assistant = messages
                            .iter_mut()
                            .rev()
                            .find(|m| m.role == MessageRole::Assistant && !m.tool_calls.is_empty());

                        match last_assistant {
                            None => {
                                tracing::warn!(
                                    "Orphan tool_result with no prior assistant message — skipping"
                                );
                            }
                            Some(asst_msg) => {
                                for block in tool_results {
                                    if let ClaudeCodeBlock::ToolResult {
                                        tool_use_id,
                                        content: result_content,
                                        is_error,
                                    } = block
                                    {
                                        // Find matching tool call by id
                                        let tc = asst_msg
                                            .tool_calls
                                            .iter_mut()
                                            .find(|tc| &tc.id == tool_use_id);
                                        match tc {
                                            Some(tc) => {
                                                let content_str = match result_content {
                                                    serde_json::Value::String(s) => s.clone(),
                                                    other => other.to_string(),
                                                };
                                                tc.result = Some(ToolResultInfo {
                                                    content: content_str,
                                                    is_error: is_error.unwrap_or(false),
                                                });
                                            }
                                            None => {
                                                tracing::warn!(
                                                    tool_use_id = %tool_use_id,
                                                    "tool_result references unknown tool_use_id — skipping"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "assistant" => {
                if let ClaudeCodeContent::Blocks(blocks) = content {
                    let created_at = parse_timestamp(line.timestamp.as_deref());
                    let id = line.uuid.clone().unwrap_or_else(generate_message_id);

                    let mut content_parts: Vec<String> = Vec::new();
                    let mut content_blocks: Vec<ContentBlockType> = Vec::new();
                    let mut tool_calls: Vec<ToolCallInfo> = Vec::new();

                    for block in blocks {
                        match block {
                            ClaudeCodeBlock::Text { text } => {
                                content_parts.push(text.clone());
                                content_blocks.push(ContentBlockType::Text);
                            }
                            ClaudeCodeBlock::Thinking { thinking } => {
                                content_blocks.push(ContentBlockType::Thinking(thinking.clone()));
                            }
                            ClaudeCodeBlock::ToolUse {
                                id: tu_id,
                                name,
                                input,
                            } => {
                                content_blocks.push(ContentBlockType::ToolCall);
                                tool_calls.push(ToolCallInfo {
                                    id: tu_id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                    result: None,
                                    started_at_ms: None,
                                    completed_at_ms: None,
                                    status: None,
                                });
                            }
                            ClaudeCodeBlock::Image => {
                                tracing::warn!(
                                    "Skipping image block in assistant message (v1 limitation)"
                                );
                            }
                            ClaudeCodeBlock::ToolResult { .. } | ClaudeCodeBlock::Unknown => {
                                // Unexpected in an assistant turn — skip
                                tracing::warn!(
                                    "Unexpected block type in assistant turn — skipping"
                                );
                            }
                        }
                    }

                    // Drop assistant messages that yielded nothing — e.g. turns
                    // containing only Image or Unknown blocks. Emitting them
                    // would pollute the imported conversation with empty turns.
                    if content_parts.is_empty()
                        && tool_calls.is_empty()
                        && content_blocks.is_empty()
                    {
                        tracing::warn!(
                            "Skipping empty assistant message (only unsupported blocks)"
                        );
                        continue;
                    }

                    let content = content_parts.join("\n\n");
                    messages.push(ChatMessage {
                        id,
                        role: MessageRole::Assistant,
                        content,
                        content_blocks,
                        tool_calls,
                        created_at,
                        token_count: None,
                        stop_reason: None,
                        synthetic: false,
                        images: vec![],
                        origin: crate::domain::models::ChannelKind::Terminal,
                        authorship: Default::default(),
                        retracted_at_ms: None,
                    });
                } else if let ClaudeCodeContent::Text(text) = content {
                    // Plain text assistant message (less common)
                    let created_at = parse_timestamp(line.timestamp.as_deref());
                    let id = line.uuid.clone().unwrap_or_else(generate_message_id);
                    messages.push(ChatMessage {
                        id,
                        role: MessageRole::Assistant,
                        content: text.clone(),
                        content_blocks: vec![ContentBlockType::Text],
                        tool_calls: vec![],
                        created_at,
                        token_count: None,
                        stop_reason: None,
                        synthetic: false,
                        images: vec![],
                        origin: crate::domain::models::ChannelKind::Terminal,
                        authorship: Default::default(),
                        retracted_at_ms: None,
                    });
                }
            }
            other => {
                tracing::warn!(
                    line_type = other,
                    "Unknown Claude Code line type — skipping"
                );
            }
        }
    }

    messages
}

/// Extract `ImportCandidate` metadata from a `.jsonl` file's content string.
///
/// Fast path for discovery — does NOT build a full `Vec<ChatMessage>`.
pub fn extract_candidate_metadata(
    path: &Path,
    contents: &str,
) -> Result<ImportCandidate, ImportError> {
    let source_session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut first_timestamp: Option<i64> = None;
    let mut first_user_content: Option<String> = None;
    let mut message_count: usize = 0;

    for raw_line in contents.lines() {
        let line = match parse_jsonl_line(raw_line) {
            Ok(Some(l)) => l,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("Skipping malformed JSONL line during discovery: {}", e);
                continue;
            }
        };

        // Track first timestamp
        if first_timestamp.is_none() {
            first_timestamp = line.timestamp.as_deref().and_then(parse_timestamp_str);
        }
        // Skip non-conversation lines
        match line.line_type.as_str() {
            "system" | "file-history-snapshot" | "permission-mode" => continue,
            _ => {}
        }
        if line.is_meta {
            continue;
        }

        // Count user + assistant messages
        if matches!(line.line_type.as_str(), "user" | "assistant") {
            message_count += 1;

            // Extract title from first user message (DF-134: walk blocks for block-array).
            if line.line_type == "user" && first_user_content.is_none() && !line.is_meta {
                if let Some(ref msg) = line.message {
                    match &msg.content {
                        Some(ClaudeCodeContent::Text(text)) => {
                            first_user_content = Some(text.clone());
                        }
                        Some(ClaudeCodeContent::Blocks(blocks)) => {
                            // Walk blocks to find the first Text block (DF-134).
                            let text = blocks.iter().find_map(|b| {
                                if let ClaudeCodeBlock::Text { text } = b {
                                    Some(text.clone())
                                } else {
                                    None
                                }
                            });
                            if let Some(t) = text {
                                first_user_content = Some(t);
                            }
                        }
                        None => {}
                    }
                }
            }
        }
    }

    let title = match first_user_content {
        Some(ref text) => extract_title_from_first_message(text),
        None => "Untitled".to_string(),
    };

    let created_at = first_timestamp.unwrap_or(0);

    Ok(ImportCandidate {
        source_session_id,
        title,
        created_at,
        message_count,
        source_path: path.to_path_buf(),
    })
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Parse an ISO8601 timestamp string to Unix epoch seconds.
/// Returns `0` on parse failure (do NOT panic).
pub fn parse_timestamp(ts: Option<&str>) -> i64 {
    ts.and_then(parse_timestamp_str).unwrap_or(0)
}

/// Parse an ISO8601 timestamp `&str` to Unix epoch seconds.
/// Returns `None` on parse failure.
fn parse_timestamp_str(s: &str) -> Option<i64> {
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(dt) => Some(dt.timestamp()),
        Err(e) => {
            tracing::warn!("Failed to parse timestamp {:?}: {}", s, e);
            None
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn user_line(uuid: &str, content: &str, timestamp: &str) -> ClaudeCodeLine {
        ClaudeCodeLine {
            line_type: "user".to_string(),
            uuid: Some(uuid.to_string()),
            timestamp: Some(timestamp.to_string()),
            is_meta: false,
            message: Some(ClaudeCodeMessage {
                role: Some("user".to_string()),
                content: Some(ClaudeCodeContent::Text(content.to_string())),
            }),
        }
    }

    #[test]
    fn test_parse_user_text_message() {
        let json = r#"{"type":"user","uuid":"abc-123","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":"Hello world"}}"#;
        let result = parse_jsonl_line(json).unwrap().unwrap();
        assert_eq!(result.line_type, "user");
        assert_eq!(result.uuid.as_deref(), Some("abc-123"));
        assert!(!result.is_meta);
        if let Some(ClaudeCodeContent::Text(t)) = result.message.unwrap().content {
            assert_eq!(t, "Hello world");
        } else {
            panic!("Expected text content");
        }
    }

    #[test]
    fn test_parse_user_blocks_message() {
        let json = r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"result text"}]}}"#;
        let result = parse_jsonl_line(json).unwrap().unwrap();
        if let Some(ClaudeCodeContent::Blocks(blocks)) = result.message.unwrap().content {
            assert_eq!(blocks.len(), 1);
            assert!(matches!(blocks[0], ClaudeCodeBlock::ToolResult { .. }));
        } else {
            panic!("Expected blocks content");
        }
    }

    #[test]
    fn test_parse_assistant_with_thinking_and_tool_use() {
        let json = r#"{"type":"assistant","uuid":"a1","timestamp":"2026-04-01T10:00:00Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"let me think"},{"type":"tool_use","id":"tu1","name":"Read","input":{"file_path":"/tmp/x"}}]}}"#;
        let result = parse_jsonl_line(json).unwrap().unwrap();
        if let Some(ClaudeCodeContent::Blocks(blocks)) = result.message.unwrap().content {
            assert_eq!(blocks.len(), 2);
            assert!(matches!(blocks[0], ClaudeCodeBlock::Thinking { .. }));
            assert!(matches!(blocks[1], ClaudeCodeBlock::ToolUse { .. }));
        } else {
            panic!("Expected blocks");
        }
    }

    #[test]
    fn test_convert_skips_meta_lines() {
        let meta_line = ClaudeCodeLine {
            line_type: "user".to_string(),
            uuid: Some("meta-uuid".to_string()),
            timestamp: Some("2026-04-01T10:00:00Z".to_string()),
            is_meta: true,
            message: Some(ClaudeCodeMessage {
                role: Some("user".to_string()),
                content: Some(ClaudeCodeContent::Text("injected content".to_string())),
            }),
        };
        let real_line = user_line("real-uuid", "Real message", "2026-04-01T10:01:00Z");
        let msgs = convert_lines_to_chat_messages(&[meta_line, real_line]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Real message");
    }

    #[test]
    fn test_convert_skips_system_and_snapshot_lines() {
        let system_line = ClaudeCodeLine {
            line_type: "system".to_string(),
            uuid: None,
            timestamp: None,
            is_meta: false,
            message: None,
        };
        let snapshot_line = ClaudeCodeLine {
            line_type: "file-history-snapshot".to_string(),
            uuid: None,
            timestamp: None,
            is_meta: false,
            message: None,
        };
        let real_line = user_line("u1", "Hello", "2026-04-01T10:00:00Z");
        let msgs = convert_lines_to_chat_messages(&[system_line, snapshot_line, real_line]);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn test_convert_merges_tool_result_into_prior_assistant() {
        let asst_line = ClaudeCodeLine {
            line_type: "assistant".to_string(),
            uuid: Some("a1".to_string()),
            timestamp: Some("2026-04-01T10:00:00Z".to_string()),
            is_meta: false,
            message: Some(ClaudeCodeMessage {
                role: Some("assistant".to_string()),
                content: Some(ClaudeCodeContent::Blocks(vec![ClaudeCodeBlock::ToolUse {
                    id: "tu-xyz".to_string(),
                    name: "Read".to_string(),
                    input: serde_json::json!({"file": "/tmp/x"}),
                }])),
            }),
        };
        let result_line = ClaudeCodeLine {
            line_type: "user".to_string(),
            uuid: Some("u2".to_string()),
            timestamp: Some("2026-04-01T10:01:00Z".to_string()),
            is_meta: false,
            message: Some(ClaudeCodeMessage {
                role: Some("user".to_string()),
                content: Some(ClaudeCodeContent::Blocks(vec![
                    ClaudeCodeBlock::ToolResult {
                        tool_use_id: "tu-xyz".to_string(),
                        content: serde_json::json!("file content here"),
                        is_error: Some(false),
                    },
                ])),
            }),
        };

        let msgs = convert_lines_to_chat_messages(&[asst_line, result_line]);
        // Should be 1 assistant message (tool result merged, no extra user message)
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, MessageRole::Assistant);
        let tc = &msgs[0].tool_calls[0];
        assert_eq!(tc.id, "tu-xyz");
        assert!(tc.result.is_some());
        assert_eq!(tc.result.as_ref().unwrap().content, "file content here");
    }

    #[test]
    fn test_convert_orphan_tool_result_is_skipped() {
        let result_line = ClaudeCodeLine {
            line_type: "user".to_string(),
            uuid: Some("u1".to_string()),
            timestamp: Some("2026-04-01T10:00:00Z".to_string()),
            is_meta: false,
            message: Some(ClaudeCodeMessage {
                role: Some("user".to_string()),
                content: Some(ClaudeCodeContent::Blocks(vec![
                    ClaudeCodeBlock::ToolResult {
                        tool_use_id: "orphan-id".to_string(),
                        content: serde_json::json!("orphan result"),
                        is_error: None,
                    },
                ])),
            }),
        };
        let msgs = convert_lines_to_chat_messages(&[result_line]);
        // No prior assistant — the user turn with only tool_result should be skipped
        assert_eq!(msgs.len(), 0);
    }

    #[test]
    fn test_extract_title_uses_first_user_message() {
        let contents = r#"{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:00Z","isMeta":true,"message":{"role":"user","content":"injected"}}
{"type":"user","uuid":"u2","timestamp":"2026-04-01T10:01:00Z","message":{"role":"user","content":"Add tab bar integration"}}
"#;
        let path = std::path::Path::new("session-uuid.jsonl");
        let candidate = extract_candidate_metadata(path, contents).unwrap();
        assert_eq!(candidate.title, "Add tab bar integration");
    }

    #[test]
    fn test_extract_title_fallback_untitled() {
        let contents = r#"{"type":"system","timestamp":"2026-04-01T10:00:00Z"}
{"type":"assistant","uuid":"a1","timestamp":"2026-04-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]}}
"#;
        let path = std::path::Path::new("session-uuid.jsonl");
        let candidate = extract_candidate_metadata(path, contents).unwrap();
        assert_eq!(candidate.title, "Untitled");
    }

    #[test]
    fn test_convert_handles_malformed_timestamp_gracefully() {
        let line = ClaudeCodeLine {
            line_type: "user".to_string(),
            uuid: Some("u1".to_string()),
            timestamp: Some("not-a-valid-timestamp".to_string()),
            is_meta: false,
            message: Some(ClaudeCodeMessage {
                role: Some("user".to_string()),
                content: Some(ClaudeCodeContent::Text("Hello".to_string())),
            }),
        };
        let msgs = convert_lines_to_chat_messages(&[line]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].created_at, 0); // fallback to 0
    }

    #[test]
    fn test_convert_ignores_image_blocks_v1() {
        let line = ClaudeCodeLine {
            line_type: "assistant".to_string(),
            uuid: Some("a1".to_string()),
            timestamp: Some("2026-04-01T10:00:00Z".to_string()),
            is_meta: false,
            message: Some(ClaudeCodeMessage {
                role: Some("assistant".to_string()),
                content: Some(ClaudeCodeContent::Blocks(vec![
                    ClaudeCodeBlock::Text {
                        text: "Here is an image:".to_string(),
                    },
                    ClaudeCodeBlock::Image,
                ])),
            }),
        };
        let msgs = convert_lines_to_chat_messages(&[line]);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].images.is_empty());
        assert_eq!(msgs[0].content, "Here is an image:");
    }

    #[test]
    fn test_convert_preserves_message_uuid_as_chat_message_id() {
        let line = user_line("my-stable-uuid", "Hello", "2026-04-01T10:00:00Z");
        let msgs = convert_lines_to_chat_messages(&[line]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "my-stable-uuid");
    }

    #[test]
    fn test_parse_blank_line_returns_none() {
        assert!(parse_jsonl_line("").unwrap().is_none());
        assert!(parse_jsonl_line("   ").unwrap().is_none());
    }

    #[test]
    fn test_parse_malformed_json_returns_error() {
        let result = parse_jsonl_line("{not valid json}");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_candidate_metadata_from_fixture() {
        let contents = r#"{"type":"file-history-snapshot","timestamp":"2026-04-01T09:00:00Z"}
{"type":"user","uuid":"u1","timestamp":"2026-04-01T10:00:00Z","message":{"role":"user","content":"How do I fork a conversation?"}}
{"type":"assistant","uuid":"a1","timestamp":"2026-04-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"You can fork by pressing F"}]}}
{"type":"user","uuid":"u2","timestamp":"2026-04-01T10:02:00Z","message":{"role":"user","content":"Thanks!"}}
"#;
        let path = std::path::Path::new("my-session-id.jsonl");
        let candidate = extract_candidate_metadata(path, contents).unwrap();
        assert_eq!(candidate.source_session_id, "my-session-id");
        assert_eq!(candidate.title, "How do I fork a conversation?");
        assert_eq!(candidate.message_count, 3); // 2 user + 1 assistant
        assert!(candidate.created_at > 0);
    }
}
