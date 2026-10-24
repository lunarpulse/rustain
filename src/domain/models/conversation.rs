#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::checkpoint::CheckpointId;
use super::content::ContentBlockType;
use super::message::MessageRole;
use super::provenance::{Authorship, SendAuthorization};
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

/// Host-owned lineage distinguishing ordinary local history from peer-display evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistorySource {
    #[default]
    Local,
    PeerDisplay,
}

fn is_local_history_source(source: &HistorySource) -> bool {
    *source == HistorySource::Local
}

/// A single message in a conversation. Persisted to session files.
/// Distinct from `Message` which is the provider-agnostic API request format.
///
/// `Default` (Story 12.2b) lets new daemon/channel code build a message with
/// `ChatMessage { content, role, origin, ..Default::default() }` instead of
/// restating every field; `MessageRole`'s own `Default` (Assistant) and
/// `ChannelKind`'s (`Terminal`) back it.
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
    /// Host-owned distinction for peer-display history; Local remains legacy-compatible.
    #[serde(default, skip_serializing_if = "is_local_history_source")]
    pub history_source: HistorySource,
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
    /// Originating channel (Story 12.2b AC5). Intrinsic provenance — persisted so a
    /// re-attached / crash-replayed message still renders its `[telegram]`/`[cron]`
    /// prefix. `#[serde(default)]` reads pre-12.2b sessions (no `origin` key) as
    /// [`ChannelKind::Terminal`]; the default is omitted from JSON so unchanged
    /// terminal-only sessions produce byte-identical files (snapshot stability).
    #[serde(default, skip_serializing_if = "is_terminal_origin")]
    pub origin: super::channel_kind::ChannelKind,
    /// Agent-composed text remains marked even after operator edits.
    #[serde(
        default = "Authorship::human_written",
        skip_serializing_if = "Authorship::is_human_written"
    )]
    pub authorship: Authorship,
    /// AD-1830 fact 2, recorded at the act, never inferred (Story 19.20a).
    #[serde(
        default = "SendAuthorization::unknown",
        skip_serializing_if = "SendAuthorization::is_unknown"
    )]
    pub send_authorization: SendAuthorization,
    /// Host-local append-only retraction mark; content is intentionally retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retracted_at_ms: Option<i64>,
}
impl Default for ChatMessage {
    fn default() -> Self {
        Self {
            id: String::new(),
            role: Default::default(),
            content: String::new(),
            content_blocks: Vec::new(),
            tool_calls: Vec::new(),
            created_at: 0,
            token_count: None,
            history_source: Default::default(),
            stop_reason: None,
            synthetic: false,
            images: Vec::new(),
            origin: Default::default(),
            authorship: Authorship::HumanWritten,
            send_authorization: SendAuthorization::Unknown,
            retracted_at_ms: None,
        }
    }
}

impl ChatMessage {
    /// Explicit or structurally retained legacy peer-display lineage.
    pub fn is_peer_display(&self) -> bool {
        self.history_source == HistorySource::PeerDisplay
            || self.content_blocks.contains(&ContentBlockType::PeerText)
    }
}

/// `skip_serializing_if` predicate: the `Terminal` default is omitted from JSON so
/// pre-12.2b terminal-only session files round-trip byte-identically.
fn is_terminal_origin(origin: &super::channel_kind::ChannelKind) -> bool {
    *origin == super::channel_kind::ChannelKind::Terminal
}

/// Generate a unique message ID using nanoid.
pub fn generate_message_id() -> String {
    nanoid::nanoid!()
}

/// Host-owned state for a persisted peer response row. Peer body text is never
/// authoritative for the target, type, controller stage, or action capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerRowStage {
    AwaitingOperator,
    Drafting,
    Ready,
    Settled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerRowControl {
    pub node_id: String,
    pub item_address: Option<super::ItemAddress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stories: Option<super::StorySummary>,
    pub message_type: super::SemanticMessageType,
    pub sender_label: String,
    pub stage: PeerRowStage,
    /// Recomputed from the live controller when producing an attach snapshot.
    pub node_resolvable: bool,
}

/// One committed mutation; revisions are host-owned and strictly increasing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PeerRowOperation {
    Upsert {
        revision: u64,
        message: ChatMessage,
        control: Option<PeerRowControl>,
    },
    Remove {
        revision: u64,
        row_id: String,
    },
}

impl PeerRowOperation {
    pub const fn revision(&self) -> u64 {
        match self {
            Self::Upsert { revision, .. } | Self::Remove { revision, .. } => *revision,
        }
    }
}

fn peer_row_revision_is_zero(revision: &u64) -> bool {
    *revision == 0
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
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub peer_row_controls: std::collections::BTreeMap<String, PeerRowControl>,
    #[serde(default, skip_serializing_if = "peer_row_revision_is_zero")]
    pub peer_row_revision: u64,
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
        let old_sources: std::collections::HashSet<String> = self
            .messages
            .iter()
            .filter(|m| m.is_peer_display())
            .map(|m| m.id.clone())
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

        for turn in &mut self.turns {
            if old_sources.contains(&turn.id.0) {
                turn.history_source = HistorySource::PeerDisplay;
            }
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
                                diff: output.diff.clone(),
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
            let provenance = msg_indices.get(&turn.id.0).map(|&idx| {
                let previous = &self.messages[idx];
                (
                    previous.authorship,
                    previous.send_authorization,
                    previous.retracted_at_ms,
                    previous.origin,
                    previous.synthetic,
                )
            });

            let msg = ChatMessage {
                id: turn.id.0.clone(),
                role: turn.role,
                content,
                content_blocks: old_blocks.remove(&turn.id.0).unwrap_or_default(),
                tool_calls,
                created_at,
                history_source: turn.history_source,
                token_count: self.usage.as_ref().map(|u| u.output_tokens),
                stop_reason: turn.stop_reason.clone(),
                synthetic: provenance.is_some_and(|facts| facts.4),
                images: vec![],
                origin: provenance.map_or(crate::domain::models::ChannelKind::Terminal, |facts| {
                    facts.3
                }),
                authorship: provenance.map_or(Authorship::HumanWritten, |facts| facts.0),
                send_authorization: provenance.map_or(SendAuthorization::Unknown, |facts| facts.1),
                retracted_at_ms: provenance.and_then(|facts| facts.2),
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
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub peer_row_controls: std::collections::BTreeMap<String, PeerRowControl>,
    #[serde(default, skip_serializing_if = "peer_row_revision_is_zero")]
    pub peer_row_revision: u64,
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
            peer_row_controls: conv.peer_row_controls.clone(),
            peer_row_revision: conv.peer_row_revision,
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
            peer_row_controls: self.peer_row_controls,
            peer_row_revision: self.peer_row_revision,
        };

        // Legacy-deserialization fallback: if `turns` is empty but `messages` is populated
        // (pre-16.2 session JSON), populate `turns` by migrating each Assistant `ChatMessage`.
        // Non-Assistant messages are kept in `messages` only (rebuild_messages_mirror preserves
        // them) — migrating them to `turns` would cause them to be dropped by the mirror rebuild
        // because rebuild_messages_mirror only rebuilds Assistant messages from turns.
        if conv.turns.is_empty() && !conv.messages.is_empty() {
            for msg in &mut conv.messages {
                if msg.role == MessageRole::Assistant {
                    let turn = migrate_chat_message_to_turn(msg);
                    // Story 19.20a: id-less legacy rows acquire the same unique join identity.
                    if msg.id.is_empty() {
                        msg.id = turn.id.0.clone();
                    }
                    // The turn renderer and rebuilt mirror use these invocation identities.
                    // Normalize immediately so selection/copy work before the first rebuild.
                    for (call, part_id) in msg.tool_calls.iter_mut().zip(
                        turn.parts.iter().filter_map(|part| match part {
                            TurnPart::ToolInvocation { id, .. } => Some(*id),
                            _ => None,
                        }),
                    ) {
                        call.id = tool_call_id_for(&turn.id, part_id);
                    }
                    conv.turns.push(turn);
                }
            }
        }

        conv
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_migration_gives_idless_rows_distinct_mirror_join_identities() {
        let mut conversation = Conversation::default();
        conversation.messages = vec![
            ChatMessage {
                role: MessageRole::Assistant,
                content: "first".into(),
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::Automatic,
                ..Default::default()
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: "second".into(),
                retracted_at_ms: Some(123),
                ..Default::default()
            },
        ];
        let mut loaded = PersistedConversation::from_conversation(&conversation).to_conversation();
        assert_ne!(loaded.messages[0].id, loaded.messages[1].id);
        for (message, turn) in loaded.messages.iter().zip(&loaded.turns) {
            assert!(!message.id.is_empty());
            assert_eq!(message.id, turn.id.0);
        }
        loaded.rebuild_messages_mirror();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].authorship, Authorship::AgentComposed);
        assert_eq!(
            loaded.messages[0].send_authorization,
            SendAuthorization::Automatic
        );
        assert_eq!(loaded.messages[1].retracted_at_ms, Some(123));
    }

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
            history_source: HistorySource::Local,
            stop_reason: None,
            synthetic: false,
            images,
            origin: crate::domain::models::ChannelKind::Terminal,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
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
        assert_eq!(msg.history_source, HistorySource::Local);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("historySource"));
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
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
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
                sub_tasks: vec![],
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

    #[test]
    fn legacy_peer_text_remains_tainted_after_mirror_rebuild_and_persisted_restore() {
        let mut conversation = Conversation {
            id: "lineage".into(),
            title: "lineage".into(),
            messages: vec![ChatMessage {
                id: "assistant-turn".into(),
                content: "peer-authored".into(),
                content_blocks: vec![ContentBlockType::PeerText],
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::Automatic,
                retracted_at_ms: Some(123456),
                ..Default::default()
            }],
            turns: vec![],
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: Default::default(),
            fork_source: None,
            compaction: None,
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
        };
        let persisted = PersistedConversation::from_conversation(&conversation);
        let restored = persisted.to_conversation();
        assert_eq!(restored.turns[0].history_source, HistorySource::PeerDisplay);
        conversation.turns = restored.turns;
        conversation.rebuild_messages_mirror();
        assert_eq!(
            conversation.messages[0].authorship,
            Authorship::AgentComposed
        );
        assert_eq!(
            conversation.messages[0].send_authorization,
            SendAuthorization::Automatic
        );
        assert_eq!(conversation.messages[0].retracted_at_ms, Some(123456));
        assert_eq!(conversation.messages[0].content, "peer-authored");
        assert_eq!(
            conversation.messages[0].history_source,
            HistorySource::PeerDisplay
        );

        let ordinary = ChatMessage {
            content: "ordinary".into(),
            authorship: Authorship::AgentComposed,
            ..Default::default()
        };
        assert!(!ordinary.is_peer_display());
    }

    #[test]
    fn auto_sent_authorship_and_retraction_round_trip_without_legacy_noise() {
        let legacy = make_chat_message(vec![]);
        let legacy_json = serde_json::to_string(&legacy).unwrap();
        assert!(!legacy_json.contains("authorship"));
        assert!(!legacy_json.contains("retractedAtMs"));

        let mut marked = legacy;
        marked.authorship = Authorship::AgentComposed;
        marked.retracted_at_ms = Some(1_700_000_123_000);
        let json = serde_json::to_string(&marked).unwrap();
        assert!(json.contains("\"authorship\":\"agent-composed\""));
        assert!(json.contains("\"retractedAtMs\":1700000123000"));
        let restored: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.authorship, Authorship::AgentComposed);
        assert_eq!(restored.retracted_at_ms, Some(1_700_000_123_000));
    }
}
