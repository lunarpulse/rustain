#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::checkpoint::CheckpointId;
use super::content::ContentBlockType;
use super::message::MessageRole;
use super::stream::StopReason;
use super::tools::{ToolCallInfo, ToolResultInfo};
use super::turn::{Turn, TurnPart, migrate_chat_message_to_turn, tool_call_id_for};
use super::usage::UsageInfo;

/// Returns `true` only if `s` matches the hash-addressed filename format produced by
/// `content_hash`: exactly 16 lowercase hex chars, a dot, and a known extension.
///
/// Validated at deserialization time to block path traversal via crafted session files
/// (party-mode review finding D1, 2026-04-12). The constraint is tight by design —
/// only `persist_image_attachments` ever creates these names.
fn is_valid_image_file_name(s: &str) -> bool {
    let Some(dot) = s.find('.') else { return false };
    let (name, rest) = s.split_at(dot);
    let ext = &rest[1..]; // skip the dot
    name.len() == 16
        && name.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        && matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "webp" | "bin")
}

fn deserialize_image_file_name<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    if is_valid_image_file_name(&s) {
        Ok(s)
    } else {
        Err(serde::de::Error::custom(format!(
            "invalid ImageReference.file_name {:?}: expected \
             '{{16 lowercase hex chars}}.{{png|jpg|jpeg|gif|webp|bin}}'",
            s
        )))
    }
}

/// A reference to an image file persisted on disk in the session's `images/` directory.
///
/// Stored on `ChatMessage.images` so image attachments survive a reload. Pairs with the
/// hash-addressed file `{sessions_dir}/{conversation_id}/images/{file_name}`.
/// See Story 4-3a.1 (DF-067).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImageReference {
    /// Hash-based filename (e.g., "a1b2c3d4e5f60708.png"). Resolved against the
    /// session's images/ directory.
    /// Validated at deserialization: must be 16 hex chars + known extension to prevent
    /// path traversal via crafted session files (D1, party-mode review 2026-04-12).
    #[serde(deserialize_with = "deserialize_image_file_name")]
    pub file_name: String,
    /// MIME type (e.g., "image/png", "image/jpeg").
    pub media_type: String,
    /// Original file size in bytes (post base64-decode).
    pub original_size: usize,
}

/// A single message in a conversation. Persisted to session files.
/// Distinct from `Message` which is the provider-agnostic API request format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// Unique message ID (nanoid). `#[serde(default)]` ensures old sessions without IDs still load.
    #[serde(default = "generate_message_id")]
    pub id: String,
    pub role: super::message::MessageRole,
    pub content: String,
    pub content_blocks: Vec<ContentBlockType>,
    pub tool_calls: Vec<ToolCallInfo>,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    pub token_count: Option<u32>,
    /// Why this message's generation stopped. `None` for user messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// Whether this message was synthesized by the system (e.g., mode handoff).
    /// `true` for synthetic messages; `false` for user-typed messages.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthetic: bool,
    /// Image attachments persisted alongside this message.
    /// Empty vec is omitted from JSON output for backward compatibility with pre-4.3a.1 sessions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageReference>,
}

/// Generate a unique message ID using nanoid.
pub fn generate_message_id() -> String {
    nanoid::nanoid!()
}

/// Persistable conversation data.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
    /// Authoritative turn-level state. Committed by `reduce()` on TurnComplete.
    /// `messages` is a legacy mirror rebuilt from `turns` after each commit via
    /// `rebuild_messages_mirror` — to be removed in S16.4 alongside `ChatMessage`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<Turn>,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    /// Unix timestamp in seconds.
    pub updated_at: i64,
    /// Unix timestamp in seconds.
    pub last_response_at: Option<i64>,
    pub session_id: Option<String>,
    pub usage: Option<UsageInfo>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub plans: std::collections::HashMap<String, super::plan::Plan>,
    pub fork_source: Option<ForkSource>,
    pub compaction: Option<CompactionState>,
    // v0.5+: pub active_agent: Option<AgentDefinition>,
    // v1.0+: pub enabled_mcp_servers: Vec<String>,
    // v1.0+: pub external_context_paths: Vec<String>,
}

impl Conversation {
    /// Rebuild the legacy `messages` mirror from the authoritative `turns`.
    ///
    /// Preserves non-Assistant messages (User, System) that are not yet represented
    /// in `turns` (S16.4 migrates those). Only rebuilds Assistant messages from turns.
    ///
    /// Path B: This deliberately reproduces the legacy concat-then-tools shape
    /// (the bug we are fixing) so that chat_pane, bookmark_list, search, export,
    /// and all other `ChatMessage` consumers keep working unchanged. Removed in
    /// S16.4 alongside the chat_pane render flip.
    // TODO(S16.4): delete this method and the `messages` mirror
    pub fn rebuild_messages_mirror(&mut self) {
        // Preserve content_blocks from old messages so plan cards, tool
        // blocks, etc survive across rebuilds (S16.2 spec: existing readers
        // must keep working).
        let mut old_blocks: std::collections::HashMap<String, Vec<ContentBlockType>> = self
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .filter(|m| !m.content_blocks.is_empty())
            .map(|m| (m.id.clone(), m.content_blocks.clone()))
            .collect();

        // Build a map of existing assistant message indices by id for in-place replacement
        let msg_indices: std::collections::HashMap<String, usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == MessageRole::Assistant)
            .map(|(i, m)| (m.id.clone(), i))
            .collect();

        // Track which turn ids were processed (for zombie cleanup)
        let mut processed_turn_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for turn in &self.turns {
            if turn.role != MessageRole::Assistant {
                continue;
            }

            let mut content = String::new();
            let mut tool_calls = Vec::new();
            let mut tool_results: std::collections::HashMap<super::turn::PartId, &TurnPart> =
                std::collections::HashMap::new();

            // First pass: collect results, prose, and reasoning
            for part in &turn.parts {
                match part {
                    TurnPart::Prose { text, .. } => {
                        content.push_str(text);
                    }
                    TurnPart::Reasoning { text, .. } => {
                        content.push_str(text);
                    }
                    TurnPart::ToolResult { refs, .. } => {
                        if tool_results.insert(*refs, part).is_some() {
                            tracing::warn!(
                                "Duplicate ToolResult refs {:?} in turn {} — earlier result overwritten",
                                refs,
                                turn.id.0
                            );
                        }
                    }
                    _ => {}
                }
            }

            // Second pass: build tool_calls from invocations, matching results
            for part in &turn.parts {
                if let TurnPart::ToolInvocation {
                    id: pid,
                    tool,
                    args,
                    status,
                    started_at,
                    ended_at,
                } = part
                {
                    let status_str = match status {
                        super::turn::InvocationStatus::Success => Some("✓ Success"),
                        super::turn::InvocationStatus::Error => Some("✗ Error"),
                        super::turn::InvocationStatus::Running
                        | super::turn::InvocationStatus::Pending => None,
                        super::turn::InvocationStatus::Cancelled => Some("⊘ Cancelled"),
                    };

                    let result = tool_results.get(pid).and_then(|rp| {
                        if let TurnPart::ToolResult { output, .. } = rp {
                            Some(ToolResultInfo {
                                content: output.content.clone(),
                                is_error: output.is_error,
                            })
                        } else {
                            None
                        }
                    });

                    let tc_id = tool_call_id_for(&turn.id, *pid);

                    tool_calls.push(ToolCallInfo {
                        id: tc_id,
                        name: tool.clone(),
                        input: args.clone(),
                        result,
                        started_at_ms: Some((*started_at).max(0) as u64),
                        completed_at_ms: ended_at.map(|v| v.max(0) as u64),
                        status: status_str.map(|s| s.to_string()),
                    });
                }
            }

            let created_at = turn.started_at / 1000;

            let msg = ChatMessage {
                id: turn.id.0.clone(),
                role: turn.role,
                content,
                content_blocks: old_blocks.remove(&turn.id.0).unwrap_or_default(),
                tool_calls,
                created_at,
                token_count: self.usage.as_ref().map(|u| u.output_tokens),
                stop_reason: turn.stop_reason.clone(),
                synthetic: false,
                images: vec![],
            };

            if let Some(&idx) = msg_indices.get(&turn.id.0) {
                self.messages[idx] = msg;
            } else {
                self.messages.push(msg);
            }
            processed_turn_ids.insert(turn.id.0.clone());
        }

        // Remove zombie assistant messages (turns that no longer exist)
        self.messages.retain(|m| {
            if m.role != MessageRole::Assistant {
                return true;
            }
            processed_turn_ids.contains(&m.id)
        });
    }
}

/// Summary for conversation list display (lighter than full Conversation).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    /// Unix timestamp in seconds.
    pub updated_at: i64,
    pub message_count: usize,
    /// Whether this conversation is a fork (mirrors `SessionMeta.fork_source.is_some()`).
    /// Used by the sidebar to render a fork indicator without touching disk.
    /// `#[serde(default)]` for backward compat with pre-4-3a.1 session metas.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_fork_source: bool,
}

/// Tracks the origin of a forked conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSource {
    pub conversation_id: String,
    pub message_index: usize,
    /// Checkpoint identifier at the fork point.
    /// Non-optional per Amendment 2.  Pre-existing fork entries that lack this
    /// field deserialize to `CheckpointId(0)` via the `default` helper.
    #[serde(default = "default_checkpoint_id")]
    pub checkpoint_id: CheckpointId,
}

fn default_checkpoint_id() -> CheckpointId {
    CheckpointId(0)
}

/// State tracking a compaction operation on a conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompactionState {
    pub summary: String,
    pub first_kept_message_id: String,
    pub compacted_at: i64,
    pub pre_compaction_tokens: u32,
}

/// Generate a unique conversation ID using nanoid.
pub fn generate_conversation_id() -> String {
    nanoid::nanoid!()
}

// ── PersistedConversation serde wrapper ────────────────────────────

/// CC-compatible on-disk format with camelCase JSON field naming.
/// All optional fields use `#[serde(default)]` for forward compatibility.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedConversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
    /// Authoritative turn-level state. Written alongside `messages` (harmless
    /// redundancy for one story's duration). On deserialization: if present,
    /// authoritative; if absent (pre-16.2 session), populated via `migrate_chat_message_to_turn`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<Turn>,
    pub created_at: i64,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub fork_source: Option<ForkSource>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub last_response_at: Option<i64>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub plans: std::collections::HashMap<String, super::plan::Plan>,
    /// Story 7.4: compaction state for context-window management.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionState>,
    /// Crash detection flag: `false` while session is in-flight, `true` after graceful shutdown.
    /// Defaults to `false` for forward compat (old files trigger recovery prompt — safe default).
    #[serde(default)]
    pub clean_exit: bool,
}

impl PersistedConversation {
    pub fn from_conversation(conv: &Conversation) -> Self {
        Self::from_conversation_with_exit(conv, false)
    }

    pub fn from_conversation_with_exit(conv: &Conversation, clean_exit: bool) -> Self {
        Self {
            id: conv.id.clone(),
            title: conv.title.clone(),
            messages: conv.messages.clone(),
            turns: conv.turns.clone(),
            created_at: conv.created_at,
            session_id: conv.session_id.clone(),
            fork_source: conv.fork_source.clone(),
            updated_at: Some(conv.updated_at),
            last_response_at: conv.last_response_at,
            usage: conv.usage.clone(),
            plans: conv.plans.clone(),
            compaction: conv.compaction.clone(),
            clean_exit,
        }
    }

    pub fn to_conversation(self) -> Conversation {
        let mut conv = Conversation {
            id: self.id,
            title: self.title,
            messages: self.messages,
            turns: self.turns,
            created_at: self.created_at,
            updated_at: self.updated_at.unwrap_or(self.created_at),
            last_response_at: self.last_response_at,
            session_id: self.session_id,
            usage: self.usage,
            plans: self.plans,
            fork_source: self.fork_source,
            compaction: self.compaction,
        };

        // Legacy-deserialization fallback: if `turns` is empty but `messages` is populated
        // (pre-16.2 session JSON), populate `turns` by migrating each Assistant `ChatMessage`.
        // Non-Assistant messages are kept in `messages` only (rebuild_messages_mirror preserves
        // them) — migrating them to `turns` would cause them to be dropped by the mirror rebuild
        // because rebuild_messages_mirror only rebuilds Assistant messages from turns.
        if conv.turns.is_empty() && !conv.messages.is_empty() {
            for msg in &conv.messages {
                if msg.role == MessageRole::Assistant {
                    conv.turns.push(migrate_chat_message_to_turn(msg));
                }
            }
        }

        conv
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::MessageRole;

    fn make_image_ref(name: &str) -> ImageReference {
        ImageReference {
            file_name: name.to_string(),
            media_type: "image/png".to_string(),
            original_size: 1024,
        }
    }

    fn make_chat_message(images: Vec<ImageReference>) -> ChatMessage {
        ChatMessage {
            id: "msg-test".to_string(),
            role: MessageRole::User,
            content: "hello".to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1700000000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images,
        }
    }

    // Task 1.5: ChatMessage with images serializes/deserializes correctly
    #[test]
    fn test_chat_message_with_images_roundtrip() {
        // Use valid 16-char hex filenames (required by deserialize_image_file_name validator)
        let images = vec![
            make_image_ref("a1b2c3d4e5f60708.png"),
            ImageReference {
                file_name: "b2c3d4e5f607080a.jpg".to_string(),
                media_type: "image/jpeg".to_string(),
                original_size: 2048,
            },
        ];
        let msg = make_chat_message(images.clone());
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.images.len(), 2);
        assert_eq!(deserialized.images[0].file_name, "a1b2c3d4e5f60708.png");
        assert_eq!(deserialized.images[0].media_type, "image/png");
        assert_eq!(deserialized.images[0].original_size, 1024);
        assert_eq!(deserialized.images[1].file_name, "b2c3d4e5f607080a.jpg");
        assert_eq!(deserialized.images[1].media_type, "image/jpeg");
    }

    // Task 1.6: empty images vec is omitted from JSON output (skip_serializing_if)
    #[test]
    fn test_chat_message_empty_images_omitted_from_json() {
        let msg = make_chat_message(vec![]);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains("\"images\""),
            "empty images vec must be omitted from JSON: {}",
            json
        );
    }

    // Task 1.7: existing JSON without `images` field deserializes with empty Vec (backward compat)
    #[test]
    fn test_chat_message_backward_compat_without_images_field() {
        // Legacy JSON from pre-4.3a.1 sessions — no `images` field at all
        let json = r#"{
            "id": "legacy-msg",
            "role": "user",
            "content": "legacy content",
            "contentBlocks": [],
            "toolCalls": [],
            "createdAt": 1700000000,
            "tokenCount": null
        }"#;
        let msg: ChatMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, "legacy-msg");
        assert!(msg.images.is_empty());
    }

    // camelCase field names for ImageReference
    #[test]
    fn test_image_reference_camel_case_serialization() {
        let image_ref = ImageReference {
            file_name: "a1b2c3d4e5f60708.png".to_string(),
            media_type: "image/png".to_string(),
            original_size: 5000,
        };
        let json = serde_json::to_string(&image_ref).unwrap();
        // camelCase expected due to ChatMessage's serde(rename_all)? No — ImageReference
        // has its own derive. We set rename_all = "camelCase" on it too.
        assert!(
            json.contains("\"fileName\""),
            "expected camelCase: {}",
            json
        );
        assert!(
            json.contains("\"mediaType\""),
            "expected camelCase: {}",
            json
        );
        assert!(
            json.contains("\"originalSize\""),
            "expected camelCase: {}",
            json
        );
    }

    // D1 (party-mode review 2026-04-12): deserializer rejects path-traversal filenames
    #[test]
    fn test_image_reference_rejects_path_traversal_filename() {
        let json =
            r#"{"fileName":"../../../etc/passwd","mediaType":"image/png","originalSize":100}"#;
        let result: serde_json::Result<ImageReference> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "path traversal filename must be rejected by deserializer"
        );
    }

    // D1: empty filename rejected (also covers F6 — empty file_name validation)
    #[test]
    fn test_image_reference_rejects_empty_filename() {
        let json = r#"{"fileName":"","mediaType":"image/png","originalSize":100}"#;
        let result: serde_json::Result<ImageReference> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "empty filename must be rejected by deserializer"
        );
    }

    // D1: valid hash-addressed filename accepted (regression guard)
    #[test]
    fn test_image_reference_accepts_valid_hash_filename() {
        for name in &[
            "a1b2c3d4e5f60708.png",
            "deadbeefcafe1234.jpg",
            "0123456789abcdef.gif",
            "ffffffffffffffff.webp",
            "0000000000000000.bin",
        ] {
            let json = format!(
                r#"{{"fileName":"{}","mediaType":"image/png","originalSize":100}}"#,
                name
            );
            let result: serde_json::Result<ImageReference> = serde_json::from_str(&json);
            assert!(result.is_ok(), "valid filename {:?} must be accepted", name);
        }
    }

    // Story 6-1a AC1/AC4: pre-6-1a session JSON without `plans` field deserializes to empty HashMap
    #[test]
    fn test_conversation_backward_compat_without_plans_field() {
        let json = r#"{
            "id": "legacy-conv",
            "title": "Legacy",
            "messages": [],
            "createdAt": 1700000000,
            "updatedAt": 1700000000
        }"#;
        let conv: Conversation = serde_json::from_str(json).unwrap();
        assert!(conv.plans.is_empty());
    }

    // Story 6-1a AC4: round-trip with one plan inserted preserves the data
    #[test]
    fn test_conversation_round_trip_with_plan() {
        use crate::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus};
        let mut conv = Conversation {
            id: "c1".to_string(),
            title: "Test".to_string(),
            messages: vec![],
            turns: Vec::new(),
            created_at: 1700000000,
            updated_at: 1700000000,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
        };
        let plan = Plan {
            id: "p1".to_string(),
            title: "Plan".to_string(),
            tasks: vec![PlanTask {
                number: 1,
                title: "Task".to_string(),
                description: String::new(),
                depends_on: vec![],
                status: PlanTaskStatus::Pending,
                started_at_ms: None,
                completed_at_ms: None,
                result: None,
                error: None,
                waiting_on: vec![],
                delegated_to: None,
            }],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 1700000000,
            resolved_at: None,
            host_message_id: None,
        };
        conv.plans.insert(plan.id.clone(), plan.clone());

        let persisted = PersistedConversation::from_conversation(&conv);
        let json = serde_json::to_string(&persisted).unwrap();
        let back: PersistedConversation = serde_json::from_str(&json).unwrap();
        let restored = back.to_conversation();
        assert_eq!(restored.plans.get("p1"), Some(&plan));
    }
}
