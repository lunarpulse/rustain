//! Turn — ordered stream of typed parts representing an assistant turn.
//!
//! # Why parts replace `{prose, invocations}`
//!
//! The legacy `ChatMessage` model accumulates all prose into `content: String` and all
//! tool calls into `tool_calls: Vec<ToolCallInfo>`. When the model emits
//! `text → tool → text → tool`, the interleaving is destroyed at write time — every
//! sentence that motivated a specific tool call is severed from that tool call.
//! `Turn { parts: Vec<TurnPart> }` preserves order natively.
//!
//! See ADR-16-01 §Decision §1 for the canonical data-model decision.
//!
//! # Insertion-order authority
//!
//! The reducer is single-threaded and consumes events from one channel; ordering is
//! the channel's job, not the data model's. Therefore `TurnPart` carries **no `seq`
//! field**. If we ever go multi-producer, `seq` is a non-breaking append.
//!
//! # Migration policy
//!
//! Silent in-place upgrade (ADR-16-01 §Decision §5). No `schema_version` field is
//! added anywhere. This aligns with peer implementations:
//! - **codex** — flat JSONL persistence with no `version` field (soft-renames for compat)
//! - **opencode** — SQLite via Drizzle, no per-message-table version field
//! - **gemini-cli** — export-only, irrelevant to persistence migration
//!
//! When public v1 ships, we will add `version: u32` at the transcript root plus a
//! `transcript::load()` boundary and a `LegacyBlock` variant.
//!
//! # Why `i64` millis instead of `Instant`
//!
//! `std::time::Instant` is not `Serialize`, which would break the AC5 round-trip
//! golden test. Using unix-millis `i64` is consistent with `ToolCallInfo.started_at_ms`
//! precedent and `ChatMessage.created_at: i64`.
//!
//! # Cross-references
//!
//! - Story 16.2 — reducer + prose-flush rule
//! - Story 16.3 — ViewState
//! - Story 16.4 — render
//!
//! > **Name-collision watchout:** `domain::models::turn::Turn` (this module) is
//! > **unrelated** to `domain::services::turn_queue::TurnQueue` (a queue of user
//! > messages consumed by the runtime). Do not merge or rename either concept.

use serde::{Deserialize, Serialize};

use super::conversation::ChatMessage;
use super::message::MessageRole;
use super::tools::ToolCallInfo;

// ---------------------------------------------------------------------------
// PartId
// ---------------------------------------------------------------------------

/// Turn-local monotonic part identifier.
///
/// Scoped to a single `Turn` — there is **no global counter** anywhere in the crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PartId(pub u64);

// ---------------------------------------------------------------------------
// TurnId
// ---------------------------------------------------------------------------

/// Nanoid-generated turn identifier.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub String);

/// Generate a fresh `TurnId` using nanoid.
pub fn generate_turn_id() -> TurnId {
    TurnId(nanoid::nanoid!())
}

// ---------------------------------------------------------------------------
// InvocationStatus
// ---------------------------------------------------------------------------

/// Lifecycle state of a tool invocation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum InvocationStatus {
    Pending,
    Running,
    Success,
    Error,
    Cancelled,
}

// ---------------------------------------------------------------------------
// ToolOutput
// ---------------------------------------------------------------------------

/// Result payload of a completed tool invocation.
///
/// `ToolOutput` exists alongside `ToolResultInfo` in order to decouple the `turn`
/// module from the `tools` module's exact derive set. The field shapes match
/// `ToolResultInfo` (ADR-16-01 §Decision §1), but `turn` does not depend on
/// `tools`'s `Debug`/`Serialize`/`Deserialize` choices.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// TurnPart
// ---------------------------------------------------------------------------

/// A single item inside an assistant turn.
///
/// JSON shape: `{"kind":"prose","id":7,"text":"…"}` etc.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TurnPart {
    Prose {
        id: PartId,
        text: String,
    },
    ToolInvocation {
        id: PartId,
        tool: String,
        args: serde_json::Value,
        status: InvocationStatus,
        started_at: i64,
        ended_at: Option<i64>,
    },
    ToolResult {
        id: PartId,
        refs: PartId,
        output: ToolOutput,
    },
    Reasoning {
        id: PartId,
        text: String,
    },
}

// ---------------------------------------------------------------------------
// Turn
// ---------------------------------------------------------------------------

/// An assistant turn as an ordered stream of typed parts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: TurnId,
    pub parts: Vec<TurnPart>,
    /// Unix timestamp in milliseconds.
    pub started_at: i64,
    pub model: String,
    /// The role of this turn. Defaults to `Assistant`; set to `User` in `Turn::user`,
    /// `System` in `Turn::system`. When `ChatMessage` is deleted in S16.4, this field
    /// becomes the sole role discriminator — it is not a legacy holdover.
    #[serde(default, skip_serializing_if = "is_default_role")]
    pub role: MessageRole,
    /// Stop reason from the model (set on TurnComplete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<super::stream::StopReason>,
    /// Turn-local counter; not persisted.
    #[serde(skip)]
    next_part_id: u64,
}

fn is_default_role(r: &MessageRole) -> bool {
    *r == MessageRole::Assistant
}

impl Turn {
    /// Create a new empty assistant turn.
    pub fn new(model: String, started_at: i64) -> Self {
        Self {
            id: generate_turn_id(),
            parts: Vec::new(),
            started_at,
            model,
            role: MessageRole::Assistant,
            stop_reason: None,
            next_part_id: 0,
        }
    }

    /// Create a user turn with a single prose part.
    ///
    /// For new construction sites in this story (the reducer-driven path).
    /// Existing `ChatMessage`-construction sites remain untouched until S16.4.
    pub fn user(text: String, started_at: i64) -> Self {
        let mut turn = Self::new(String::new(), started_at);
        turn.role = MessageRole::User;
        if !text.is_empty() {
            turn.push_part(|id| TurnPart::Prose { id, text });
        }
        turn
    }

    /// Create a system turn with a single prose part.
    ///
    /// For new construction sites in this story (the reducer-driven path).
    /// Existing `ChatMessage`-construction sites remain untouched until S16.4.
    pub fn system(text: String, started_at: i64) -> Self {
        let mut turn = Self::new(String::new(), started_at);
        turn.role = MessageRole::System;
        if !text.is_empty() {
            turn.push_part(|id| TurnPart::Prose { id, text });
        }
        turn
    }

    /// Append a new part built from the next monotonic `PartId`.
    ///
    /// Returns the issued `PartId`.
    pub fn push_part<F>(&mut self, build: F) -> PartId
    where
        F: FnOnce(PartId) -> TurnPart,
    {
        let id = PartId(self.next_part_id);
        self.next_part_id += 1;
        self.parts.push(build(id));
        id
    }
}

// ---------------------------------------------------------------------------
// Migration helper
// ---------------------------------------------------------------------------

/// Migrate a legacy `ChatMessage` into a single `Turn`.
///
/// This is a **lossy** translation: the original interleaving was destroyed at
/// write time by the old `ChatMessage` shape (`content` wall + `tool_calls` list).
/// The resulting turn places all prose first, then all invocations, then all
/// results — matching the old rendering order.
///
/// - `model` is empty (`ChatMessage` does not carry model).
/// - `started_at` is `msg.created_at * 1000` (seconds → millis).
pub fn migrate_chat_message_to_turn(msg: &ChatMessage) -> Turn {
    let mut turn = Turn::new(String::new(), msg.created_at.saturating_mul(1000));

    // Prose part (only if content is non-empty)
    if !msg.content.is_empty() {
        turn.push_part(|id| TurnPart::Prose {
            id,
            text: msg.content.clone(),
        });
    }

    // One invocation per tool call
    for tc in &msg.tool_calls {
        let status = map_tool_call_status(tc);
        let started_at = tc.started_at_ms.map(|v| v as i64).unwrap_or(0);
        let ended_at = tc.completed_at_ms.map(|v| v as i64);

        let invocation_id = turn.push_part(|id| TurnPart::ToolInvocation {
            id,
            tool: tc.name.clone(),
            args: tc.input.clone(),
            status,
            started_at,
            ended_at,
        });

        // One result per invocation that has a result
        if let Some(ref result) = tc.result {
            turn.push_part(|id| TurnPart::ToolResult {
                id,
                refs: invocation_id,
                output: ToolOutput {
                    content: result.content.clone(),
                    is_error: result.is_error,
                },
            });
        }
    }

    turn
}

/// Map `ToolCallInfo` fields to `InvocationStatus`.
///
/// Mapping table:
/// - `result.is_some() && result.is_error` → `Error`
/// - `result.is_some() && !result.is_error` → `Success`
/// - `result.is_none() && status.is_none()` → `Pending`
/// - otherwise → `Running` (status chip present but no result yet)
fn map_tool_call_status(tc: &ToolCallInfo) -> InvocationStatus {
    match &tc.result {
        Some(result) if result.is_error => InvocationStatus::Error,
        Some(_) => InvocationStatus::Success,
        None if tc.status.is_none() => InvocationStatus::Pending,
        None => InvocationStatus::Running,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{MessageRole, ToolResultInfo};

    // -----------------------------------------------------------------------
    // PartId
    // -----------------------------------------------------------------------

    #[test]
    fn partid_serializes_as_number() {
        assert_eq!(serde_json::to_string(&PartId(7)).unwrap(), "7");
    }

    #[test]
    fn partid_deserializes_from_number() {
        let id: PartId = serde_json::from_str("7").unwrap();
        assert_eq!(id, PartId(7));
    }

    // -----------------------------------------------------------------------
    // TurnPart JSON shape
    // -----------------------------------------------------------------------

    #[test]
    fn turnpart_prose_json_shape() {
        let part = TurnPart::Prose {
            id: PartId(0),
            text: "hi".into(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert_eq!(json, r#"{"kind":"prose","id":0,"text":"hi"}"#);
    }

    // -----------------------------------------------------------------------
    // Turn::push_part
    // -----------------------------------------------------------------------

    #[test]
    fn push_part_issues_monotonic_ids_and_appends_in_order() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        let id1 = turn.push_part(|id| TurnPart::Prose {
            id,
            text: "a".into(),
        });
        let id2 = turn.push_part(|id| TurnPart::Prose {
            id,
            text: "b".into(),
        });
        let id3 = turn.push_part(|id| TurnPart::Prose {
            id,
            text: "c".into(),
        });

        assert_eq!(id1, PartId(0));
        assert_eq!(id2, PartId(1));
        assert_eq!(id3, PartId(2));
        assert_eq!(turn.parts.len(), 3);
    }

    // -----------------------------------------------------------------------
    // Migration helper
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_text_only_message_yields_one_prose_part() {
        let msg = ChatMessage {
            id: "msg-1".into(),
            role: MessageRole::Assistant,
            content: "Hello world".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        };
        let turn = migrate_chat_message_to_turn(&msg);
        assert_eq!(turn.parts.len(), 1);
        assert!(matches!(&turn.parts[0], TurnPart::Prose { text, .. } if text == "Hello world"));
        assert_eq!(turn.started_at, 1_700_000_000);
    }

    #[test]
    fn migrate_message_with_tools_yields_prose_then_invocations_then_results() {
        let msg = ChatMessage {
            id: "msg-2".into(),
            role: MessageRole::Assistant,
            content: "Using tool".into(),
            content_blocks: vec![],
            tool_calls: vec![ToolCallInfo {
                id: "tc-1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/tmp/x"}),
                result: Some(ToolResultInfo {
                    content: "file contents".into(),
                    is_error: false,
                }),
                started_at_ms: Some(1_000),
                completed_at_ms: Some(2_000),
                status: Some("Done".into()),
            }],
            created_at: 1_700_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        };
        let turn = migrate_chat_message_to_turn(&msg);
        assert_eq!(turn.parts.len(), 3);
        assert!(matches!(&turn.parts[0], TurnPart::Prose { .. }));
        assert!(
            matches!(&turn.parts[1], TurnPart::ToolInvocation { tool, .. } if tool == "read_file")
        );
        assert!(matches!(&turn.parts[2], TurnPart::ToolResult { .. }));
    }

    #[test]
    fn migrate_message_with_failed_tool_marks_invocation_status_error() {
        let msg = ChatMessage {
            id: "msg-3".into(),
            role: MessageRole::Assistant,
            content: "Oops".into(),
            content_blocks: vec![],
            tool_calls: vec![ToolCallInfo {
                id: "tc-2".into(),
                name: "exec".into(),
                input: serde_json::json!({"cmd": "false"}),
                result: Some(ToolResultInfo {
                    content: "exit 1".into(),
                    is_error: true,
                }),
                started_at_ms: Some(1_000),
                completed_at_ms: Some(2_000),
                status: Some("Error".into()),
            }],
            created_at: 1_700_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        };
        let turn = migrate_chat_message_to_turn(&msg);
        assert!(matches!(
            &turn.parts[1],
            TurnPart::ToolInvocation {
                status: InvocationStatus::Error,
                ..
            }
        ));
    }

    #[test]
    fn migrate_empty_content_yields_zero_prose_parts() {
        let msg = ChatMessage {
            id: "msg-4".into(),
            role: MessageRole::Assistant,
            content: "".into(),
            content_blocks: vec![],
            tool_calls: vec![ToolCallInfo {
                id: "tc-3".into(),
                name: "glob".into(),
                input: serde_json::json!({"pattern": "*.rs"}),
                result: None,
                started_at_ms: None,
                completed_at_ms: None,
                status: None,
            }],
            created_at: 1_700_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        };
        let turn = migrate_chat_message_to_turn(&msg);
        // No prose part because content is empty, but invocation part exists
        assert_eq!(turn.parts.len(), 1);
        assert!(matches!(&turn.parts[0], TurnPart::ToolInvocation { .. }));
    }
}
