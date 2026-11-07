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
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

use super::PolicyPrincipal;
use super::conversation::ChatMessage;
use super::message::MessageRole;
use super::tools::ToolCallInfo;

/// Per-turn routing provenance.
///
/// This is intentionally separate from [`NodeOrigin`]: a node's origin is
/// fixed at creation while a turn can carry routing-specific payloads such as
/// an ACP session or a verified peer identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOrigin {
    Interactive,
    Acp { session_id: String },
    RemotePeer { principal: PolicyPrincipal },
    Subagent,
    Cron,
    Channel,
}
/// Per-invocation routing identity. Deliberately not serializable or persisted.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnRoute {
    Interactive,
    Acp,
    RemotePeer,
    Subagent,
    Cron,
    Channel,
}

impl TurnOrigin {
    /// Return the route without copying origin-specific or persisted data.
    pub fn route(&self) -> TurnRoute {
        match self {
            Self::Interactive => TurnRoute::Interactive,
            Self::Acp { .. } => TurnRoute::Acp,
            Self::RemotePeer { .. } => TurnRoute::RemotePeer,
            Self::Subagent => TurnRoute::Subagent,
            Self::Cron => TurnRoute::Cron,
            Self::Channel => TurnRoute::Channel,
        }
    }
}

impl TurnOrigin {
    /// Map every turn route to the corresponding node-birth origin.
    pub fn node_origin(&self) -> super::agent_node::NodeOrigin {
        match self {
            Self::Interactive => super::agent_node::NodeOrigin::Interactive,
            Self::Acp { .. } => super::agent_node::NodeOrigin::Interactive,
            Self::RemotePeer { .. } => super::agent_node::NodeOrigin::Remote,
            Self::Subagent => super::agent_node::NodeOrigin::Subagent,
            Self::Cron => super::agent_node::NodeOrigin::Cron,
            Self::Channel => super::agent_node::NodeOrigin::Channel,
        }
    }
    /// Derive the approval source for the typed route.
    ///
    /// The currently generic turn loop has no distinct approval policy for
    /// cron, channel, or subagent origins. Those routes remain explicit here
    /// so introducing a policy cannot silently fall through a wildcard.
    pub fn approval_source(&self, conversation_id: &str) -> super::tool_call::ApprovalSource {
        match self {
            Self::Interactive | Self::Subagent | Self::Cron | Self::Channel => {
                super::tool_call::ApprovalSource::ForegroundTurn {
                    conversation_id: conversation_id.to_owned(),
                }
            }
            Self::Acp { session_id } => super::tool_call::ApprovalSource::AcpSession {
                session_id: session_id.clone(),
                conversation_id: conversation_id.to_owned(),
            },
            Self::RemotePeer { principal } => {
                super::tool_call::ApprovalSource::remote_peer(conversation_id.to_owned(), principal)
            }
        }
    }

    /// Derive the tool-dispatch provenance for the typed route.
    ///
    /// Story 17.4b (R-D): a turn initiated by a remote peer carries
    /// attacker-influenceable content into the main session, so its tool
    /// dispatches are [`ProvenanceTag::SelfOriginated`] and pass through the
    /// taint gate. Every other route is [`ProvenanceTag::UserOriginated`]. This
    /// is what makes the taint gate a real control on the peer path instead of
    /// the hardcoded `UserOriginated` trapdoor at `ToolScheduler::schedule`
    /// (which 17.1b left the peer path routing through — the hole R-D closes).
    pub fn provenance(&self) -> super::taint::ProvenanceTag {
        match self {
            Self::RemotePeer { .. } => super::taint::ProvenanceTag::SelfOriginated,
            _ => super::taint::ProvenanceTag::UserOriginated,
        }
    }
}

// ---------------------------------------------------------------------------
// PartId
// ---------------------------------------------------------------------------

/// Turn-local monotonic part identifier.
///
/// Scoped to a single `Turn` — there is no global part-id counter.
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
    /// UI-only display-diff state riding the journaled tool result (Story
    /// 19.1). `#[serde(default)]` — records written before the feature load
    /// as `NotCaptured { HistoricalOrReattached }`, never as `NewFile`.
    #[serde(default)]
    pub diff: super::diff::WriteDiffState,
    /// Result images, persisted for the provider history (Story 20.1d4a T1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<super::message::ImageAttachment>,
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
    /// Operator-private provider replay state; never rendered.
    ProviderReplay {
        id: PartId,
        item: super::stream::ProviderReplayItem,
    },
}

/// Process-local source identity. Never serialized or derived from payload bytes.
pub(crate) fn next_source_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The canonical parts array, with automatic invalidation on mutable access.
///
/// A mutable borrow invalidates before exposing any nested argument/result.
/// Read-only clones retain the canonical revision; any mutable access to either
/// copy invalidates it. Deserialization gets a fresh identity, so same-id loaded
/// replacements cannot reuse stale bodies. Wire shape/equality stay array-shaped.
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnParts {
    parts: Vec<TurnPart>,
    #[serde(skip, default = "next_source_revision")]
    revision: u64,
}

impl TurnParts {
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

impl From<Vec<TurnPart>> for TurnParts {
    fn from(parts: Vec<TurnPart>) -> Self {
        Self {
            parts,
            revision: next_source_revision(),
        }
    }
}

impl Default for TurnParts {
    fn default() -> Self {
        Vec::new().into()
    }
}

impl Clone for TurnParts {
    fn clone(&self) -> Self {
        Self {
            parts: self.parts.clone(),
            revision: self.revision,
        }
    }
}

impl std::fmt::Debug for TurnParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.parts.fmt(f)
    }
}

impl PartialEq for TurnParts {
    fn eq(&self, other: &Self) -> bool {
        self.parts == other.parts
    }
}

impl Deref for TurnParts {
    type Target = Vec<TurnPart>;

    fn deref(&self) -> &Self::Target {
        &self.parts
    }
}

impl DerefMut for TurnParts {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.revision = next_source_revision();
        &mut self.parts
    }
}

impl<'a> IntoIterator for &'a TurnParts {
    type Item = &'a TurnPart;
    type IntoIter = std::slice::Iter<'a, TurnPart>;

    fn into_iter(self) -> Self::IntoIter {
        self.parts.iter()
    }
}

impl<'a> IntoIterator for &'a mut TurnParts {
    type Item = &'a mut TurnPart;
    type IntoIter = std::slice::IterMut<'a, TurnPart>;

    fn into_iter(self) -> Self::IntoIter {
        self.deref_mut().iter_mut()
    }
}

// ---------------------------------------------------------------------------
// Turn
// ---------------------------------------------------------------------------

/// An assistant turn as an ordered stream of typed parts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: TurnId,
    pub parts: TurnParts,
    /// Unix timestamp in milliseconds.
    pub started_at: i64,
    pub model: String,
    /// The role of this turn. Defaults to `Assistant`; set to `User` in `Turn::user`,
    /// `System` in `Turn::system`. When `ChatMessage` is deleted in S16.4, this field
    /// becomes the sole role discriminator — it is not a legacy holdover.
    #[serde(default, skip_serializing_if = "is_default_role")]
    pub role: MessageRole,
    /// Host-owned lineage for peer-display history; absent fields mean local history.
    #[serde(default, skip_serializing_if = "is_local_history_source")]
    pub history_source: super::conversation::HistorySource,

    /// Stop reason from the model (set on TurnComplete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<super::stream::StopReason>,
    /// One mark per provider response, in order (Story 20.1d4a T2); empty for
    /// turns recorded before it and for migrated rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub responses: Vec<ResponseMark>,
    /// Turn-local counter; not persisted.
    #[serde(skip)]
    next_part_id: u64,
}

/// The end of one provider response inside a [`Turn`] (Story 20.1d4a T2).
/// The response owns the parts from the previous mark's `end` up to its own,
/// except tool results, which belong to the invocation they reference.
/// `call_ids` are the ids the provider gave this response's invocations, in
/// part order.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResponseMark {
    pub end: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_ids: Vec<String>,
}

fn is_local_history_source(source: &super::conversation::HistorySource) -> bool {
    *source == super::conversation::HistorySource::Local
}

fn is_default_role(r: &MessageRole) -> bool {
    *r == MessageRole::Assistant
}

impl Turn {
    /// Create a new empty assistant turn.
    pub fn new(model: String, started_at: i64) -> Self {
        Self {
            id: generate_turn_id(),
            parts: TurnParts::default(),
            started_at,
            model,
            role: MessageRole::Assistant,
            history_source: super::conversation::HistorySource::Local,
            stop_reason: None,
            responses: Vec::new(),
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

    /// The mirror's display text: prose and reasoning in part order.
    pub fn display_text(&self) -> String {
        let mut text = String::new();
        for part in &self.parts {
            if let TurnPart::Prose { text: t, .. } | TurnPart::Reasoning { text: t, .. } = part {
                text.push_str(t);
            }
        }
        text
    }
}

// ---------------------------------------------------------------------------
// Canonical mirror tool-call id helper
// ---------------------------------------------------------------------------

/// Match the canonical id without formatting a temporary id for each candidate.
/// Decimal part ids must be canonical too: no leading zeroes or extra suffix.
pub fn tool_call_id_matches(turn_id: &TurnId, pid: PartId, candidate: &str) -> bool {
    candidate
        .strip_prefix("tc_")
        .and_then(|rest| rest.strip_prefix(turn_id.0.as_str()))
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix.bytes().all(|byte| byte.is_ascii_digit())
                && (suffix == "0" || !suffix.starts_with('0'))
                && suffix.parse::<u64>() == Ok(pid.0)
        })
}

/// Debug-only counts of actual canonical-id allocations on this thread.
#[cfg(any(test, debug_assertions))]
pub mod metrics {
    use std::cell::Cell;

    thread_local! {
        static IDS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn allocated_id() {
        IDS.with(|count| count.set(count.get() + 1));
    }

    pub fn allocated_ids() -> usize {
        IDS.with(Cell::get)
    }

    pub fn reset() {
        IDS.with(|count| count.set(0));
    }
}

/// Canonical mirror tool-call id format.
///
/// Used by `rebuild_messages_mirror` and the render-time `adapter_shim` so the
/// two cannot drift. The format is `"tc_{turn_id}_{part_id}"` — locked here
/// as the single source of truth.
///
/// Deleted in S16.10-cleanup alongside `Conversation.messages` mirror.
pub fn tool_call_id_for(turn_id: &TurnId, pid: PartId) -> String {
    #[cfg(any(test, debug_assertions))]
    metrics::allocated_id();
    format!("tc_{}_{}", turn_id.0, pid.0)
}

/// Whether `candidate` has the shape minted by [`block_item_id_for`]:
/// `md_<turn>_<part digits>_<ordinal digits>`; a tool id never does.
pub fn is_block_item_id(candidate: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    candidate.strip_prefix("md_").is_some_and(|rest| {
        rest.rsplit_once('_')
            .and_then(|(head, ordinal)| {
                let (turn, part) = head.rsplit_once('_')?;
                Some(!turn.is_empty() && digits(part) && digits(ordinal))
            })
            .unwrap_or(false)
    })
}

/// Stage item id of the `ordinal`-th code block or table (0-based) of a prose
/// part: `md_{turn_id}_{part_id}_{ordinal}` (Story 21.13 D7).
pub fn block_item_id_for(turn_id: &TurnId, pid: PartId, ordinal: usize) -> String {
    format!("md_{}_{}_{}", turn_id.0, pid.0, ordinal)
}

/// The ordinal when `candidate` is a canonical block id of exactly this turn
/// and part; ids are compared structurally, never by a bare prefix.
pub fn block_ordinal_of(turn_id: &TurnId, pid: PartId, candidate: &str) -> Option<usize> {
    let canonical = |digits: &str| {
        !digits.is_empty()
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && (digits == "0" || !digits.starts_with('0'))
    };
    let rest = candidate
        .strip_prefix("md_")?
        .strip_prefix(turn_id.0.as_str())?
        .strip_prefix('_')?;
    let (part, ordinal) = rest.split_once('_')?;
    (canonical(part) && canonical(ordinal) && part.parse::<u64>() == Ok(pid.0))
        .then(|| ordinal.parse().ok())?
}

/// Whether `candidate` is the id of a tool call or a code/table block of `turn`.
pub fn item_belongs_to_turn(turn: &Turn, candidate: &str) -> bool {
    turn.parts.iter().any(|part| match part {
        TurnPart::ToolInvocation { id, .. } => tool_call_id_matches(&turn.id, *id, candidate),
        TurnPart::Prose { id, .. } => block_ordinal_of(&turn.id, *id, candidate).is_some(),
        _ => false,
    })
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
    // Story 19.20a: rendering and mirror replacement join on the original row id.
    let mut turn = Turn {
        id: if msg.id.is_empty() {
            generate_turn_id()
        } else {
            TurnId(msg.id.clone())
        },
        parts: TurnParts::default(),
        started_at: msg.created_at.saturating_mul(1000),
        model: String::new(),
        role: MessageRole::Assistant,
        history_source: super::conversation::HistorySource::Local,
        stop_reason: msg.stop_reason.clone(),
        // The row's own `ResponseSpan` blocks stay on the row; the mirror keeps
        // them for a turn without marks (Story 20.1d4a T2).
        responses: Vec::new(),
        next_part_id: 0,
    };
    turn.history_source = if msg.is_peer_display() {
        super::conversation::HistorySource::PeerDisplay
    } else {
        msg.history_source
    };

    // Prose part (only if content is non-empty)
    if !msg.content.is_empty() {
        turn.push_part(|id| TurnPart::Prose {
            id,
            text: msg.content.clone(),
        });
    }

    // Story 20.1d1b: replay state written by messages-only persisters (`ask`,
    // ACP, the daemon) must survive the first turn-backed mirror rebuild.
    for block in &msg.content_blocks {
        if let super::ContentBlockType::ProviderReplay(item) = block {
            turn.push_part(|id| TurnPart::ProviderReplay {
                id,
                item: item.clone(),
            });
        }
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
        if let Some(result) = &tc.result {
            turn.push_part(|id| TurnPart::ToolResult {
                id,
                refs: invocation_id,
                output: ToolOutput {
                    content: result.content.clone(),
                    is_error: result.is_error,
                    diff: result.diff.clone(),
                    images: result.images.clone(),
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

    #[test]
    fn story21_5_parts_epochs_track_mutation_clone_and_rehydration_without_wire_changes() {
        let mut turn = Turn::new("model".into(), 0);
        turn.push_part(|id| TurnPart::ToolInvocation {
            id,
            tool: "Bash".into(),
            args: serde_json::json!({"command":"before"}),
            status: InvocationStatus::Running,
            started_at: 0,
            ended_at: None,
        });
        let revision = turn.parts.revision();
        assert_eq!(turn.parts.len(), 1);
        assert_eq!(turn.parts.revision(), revision, "reads do not invalidate");
        if let TurnPart::ToolInvocation { args, .. } = &mut turn.parts[0] {
            args["command"] = serde_json::json!("after");
        }
        assert_ne!(
            turn.parts.revision(),
            revision,
            "nested mutation invalidates"
        );
        let revision = turn.parts.revision();
        if let Some(TurnPart::ToolInvocation { status, .. }) = turn.parts.iter_mut().next() {
            *status = InvocationStatus::Success;
        }
        assert_ne!(
            turn.parts.revision(),
            revision,
            "iterator mutation invalidates"
        );
        let encoded = serde_json::to_value(&turn).unwrap();
        assert!(encoded["parts"].is_array());
        assert_eq!(
            encoded["parts"][0],
            serde_json::json!({
                "kind":"toolInvocation", "id":0, "tool":"Bash",
                "args":{"command":"after"}, "status":"success",
                "started_at":0, "ended_at":null,
            })
        );
        let restored: Turn = serde_json::from_value(encoded).unwrap();
        assert_eq!(restored.parts, turn.parts);
        assert_ne!(restored.parts.revision(), turn.parts.revision());
        let mut cloned = turn.clone();
        assert_eq!(cloned, turn);
        assert_eq!(
            cloned.parts.revision(),
            turn.parts.revision(),
            "read-only snapshots preserve their canonical source revision"
        );
        if let TurnPart::ToolInvocation { args, .. } = &mut cloned.parts[0] {
            args["command"] = serde_json::json!("clone-only-change");
        }
        assert_ne!(cloned.parts.revision(), turn.parts.revision());
        assert!(
            matches!(&turn.parts[0], TurnPart::ToolInvocation { args, .. }
            if args["command"] == "after"),
            "mutating a clone leaves its source unchanged"
        );
    }

    #[test]
    fn story21_5_canonical_id_match_is_exact_and_allocation_free() {
        let id = TurnId("turn_with_underscores".into());
        for part in [PartId(0), PartId(9), PartId(u64::MAX)] {
            let canonical = tool_call_id_for(&id, part);
            metrics::reset();
            assert!(tool_call_id_matches(&id, part, &canonical));
            assert_eq!(metrics::allocated_ids(), 0);
            for wrong in [
                format!("{canonical}_stale"),
                format!("{canonical}0"),
                canonical
                    .trim_end_matches(|c: char| c.is_ascii_digit())
                    .to_owned(),
            ] {
                assert!(!tool_call_id_matches(&id, part, &wrong), "{wrong}");
            }
        }
        assert!(!tool_call_id_matches(
            &id,
            PartId(1),
            "tc_turn_with_underscores_01"
        ));
        assert!(!tool_call_id_matches(
            &id,
            PartId(1),
            "tc_turn_with_underscores_+1"
        ));
    }

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

    #[test]
    fn remote_peer_turn_is_self_originated_every_other_route_is_user_originated() {
        use crate::domain::models::ProvenanceTag;
        let peer = crate::domain::models::PeerId::from_public_key(&[7u8; 32]).unwrap();
        // R-D: only the remote-peer route taints its tool dispatches.
        assert_eq!(
            TurnOrigin::RemotePeer {
                principal: PolicyPrincipal::rap(peer)
            }
            .provenance(),
            ProvenanceTag::SelfOriginated
        );
        for origin in [
            TurnOrigin::Interactive,
            TurnOrigin::Subagent,
            TurnOrigin::Cron,
            TurnOrigin::Channel,
            TurnOrigin::Acp {
                session_id: "s".to_owned(),
            },
        ] {
            assert_eq!(origin.provenance(), ProvenanceTag::UserOriginated);
        }
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
    fn legacy_peer_text_migration_and_turn_restore_retain_lineage() {
        use crate::domain::models::{ChatMessage, ContentBlockType, HistorySource};

        let peer = ChatMessage {
            id: "ordinary-id".into(),
            content: "peer body".into(),
            content_blocks: vec![ContentBlockType::PeerText],
            ..Default::default()
        };
        assert!(peer.is_peer_display());
        let migrated = migrate_chat_message_to_turn(&peer);
        assert_eq!(migrated.history_source, HistorySource::PeerDisplay);
        let restored: Turn =
            serde_json::from_str(&serde_json::to_string(&migrated).unwrap()).unwrap();
        assert_eq!(restored.history_source, HistorySource::PeerDisplay);

        let local = ChatMessage {
            id: "peer-response-prefix-is-not-authority".into(),
            content: "[peer response] ordinary local text".into(),
            authorship: crate::domain::models::Authorship::AgentComposed,
            ..Default::default()
        };
        assert!(!local.is_peer_display());
        assert_eq!(
            migrate_chat_message_to_turn(&local).history_source,
            HistorySource::Local
        );
    }

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
            origin: crate::domain::models::ChannelKind::Terminal,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        };
        let turn = migrate_chat_message_to_turn(&msg);
        assert_eq!(
            turn.id.0, msg.id,
            "migration retains the mirror join identity"
        );
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
                    diff: crate::domain::models::WriteDiffState::NotAWrite,
                    images: vec![],
                }),
                started_at_ms: Some(1_000),
                completed_at_ms: Some(2_000),
                status: Some("Done".into()),
                provider_id: None,
            }],
            created_at: 1_700_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin: crate::domain::models::ChannelKind::Terminal,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
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
                    diff: crate::domain::models::WriteDiffState::NotAWrite,
                    images: vec![],
                }),
                started_at_ms: Some(1_000),
                completed_at_ms: Some(2_000),
                status: Some("Error".into()),
                provider_id: None,
            }],
            created_at: 1_700_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin: crate::domain::models::ChannelKind::Terminal,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
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
                provider_id: None,
            }],
            created_at: 1_700_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin: crate::domain::models::ChannelKind::Terminal,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        };
        let turn = migrate_chat_message_to_turn(&msg);
        // No prose part because content is empty, but invocation part exists
        assert_eq!(turn.parts.len(), 1);
        assert!(matches!(&turn.parts[0], TurnPart::ToolInvocation { .. }));
    }

    #[test]
    fn story21_13_block_ids_are_structural_and_turn_membership_is_exact() {
        let mut turn = Turn::new("model".into(), 0);
        let prose = turn.push_part(|id| TurnPart::Prose {
            id,
            text: "x".into(),
        });
        let tool = turn.push_part(|id| TurnPart::ToolInvocation {
            id,
            tool: "Bash".into(),
            args: serde_json::json!({}),
            status: InvocationStatus::Success,
            started_at: 0,
            ended_at: Some(1),
        });
        let block = block_item_id_for(&turn.id, prose, 3);
        assert!(is_block_item_id(&block));
        assert_eq!(block_ordinal_of(&turn.id, prose, &block), Some(3));
        // Wrong turn, wrong part, non-canonical digits and tool ids are rejected.
        let other = TurnId("other".into());
        assert_eq!(block_ordinal_of(&other, prose, &block), None);
        assert_eq!(
            block_ordinal_of(&turn.id, PartId(prose.0 + 1), &block),
            None
        );
        for bad in ["md_x_1_", "md__1_2", "tc_x_1", "md_x_1", "md_"] {
            assert!(!is_block_item_id(bad), "{bad}");
        }
        assert_eq!(
            block_ordinal_of(
                &turn.id,
                prose,
                &format!("md_{}_{}_{}", turn.id.0, prose.0, "03")
            ),
            None
        );
        // `a` vs `a_1`: a bare prefix never decides turn membership.
        let short = TurnId("a".into());
        let long = TurnId("a_1".into());
        let id = block_item_id_for(&long, PartId(2), 0);
        assert_eq!(block_ordinal_of(&short, PartId(2), &id), None);
        assert_eq!(block_ordinal_of(&long, PartId(2), &id), Some(0));
        // Membership: the turn's own tool and block, nothing else.
        assert!(item_belongs_to_turn(&turn, &block));
        assert!(item_belongs_to_turn(
            &turn,
            &tool_call_id_for(&turn.id, tool)
        ));
        assert!(!item_belongs_to_turn(&turn, &id));
        assert!(!item_belongs_to_turn(&turn, "md_provider_tool_id"));
    }
}
