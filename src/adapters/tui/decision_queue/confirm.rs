//! Fork, rewind and export overwrite as queue items (Story 21.6 6-D, AC21).
//!
//! The payloads moved here from the old fork, rewind (index + preview) and export slots
//! and the export-overwrite focus type. All three
//! are user-started (Requested: the frame opens at once) and keyed to the conversation
//! they were started in. Fork and rewind bind the message index to a fingerprint of
//! its role and full content; the host verifies it before effects and retires targets
//! removed by truncation. Export re-stats the approved file before overwriting it.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::time::SystemTime;

use super::memory::holds_in;
use super::{Arrival, DecisionDraft, DecisionItem, DecisionKind, DecisionSubject, Raised, raise};
use crate::adapters::tui::notification_log::LogFacts;
use crate::adapters::tui::state::TuiState;

/// The longest message preview a fork card keeps (the card shows a few lines).
const PREVIEW_MAX_CHARS: usize = 2_048;

/// A fork awaiting `y`/`n` (Story 4-3a): the message the new conversation forks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPayload {
    pub conversation_id: String,
    /// Index into the conversation's messages, checked against `target_hash` at effect time.
    pub message_index: usize,
    pub target_hash: u64,
    /// The targeted message's text, bounded, for the card body.
    pub preview: String,
}

/// A single file entry in the rewind confirmation preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertPreviewItem {
    /// Workspace-relative path for display (falls back to absolute if outside workspace).
    pub display_path: String,
    /// Whether the file has been externally modified since the snapshot was taken.
    pub conflict: bool,
}

/// Pre-flight preview data for the rewind confirmation card, built without mutating state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPreview {
    /// The message index the conversation will be truncated to (inclusive).
    pub target_message_index: usize,
    /// How many messages come after `target_message_index` (will be removed).
    pub messages_to_remove: usize,
    /// Files that would be reverted, with conflict flags.
    pub files_to_revert: Vec<RevertPreviewItem>,
}

/// A rewind awaiting `y` / `f` / `n` (Story 4-3b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPayload {
    pub conversation_id: String,
    pub target_hash: u64,
    pub preview: RewindPreview,
}

/// What the host saw of the export target when the card opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
    #[cfg(unix)]
    pub dev: u64,
    #[cfg(unix)]
    pub ino: u64,
    #[cfg(unix)]
    pub ctime: i64,
    #[cfg(unix)]
    pub ctime_nsec: i64,
}

/// An `/export <name>` whose target already exists (Story 4-4 AC12): the rendered
/// conversation waits for the operator's overwrite decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOverwritePayload {
    pub conversation_id: String,
    pub target: PathBuf,
    /// The conversation rendered when `/export` ran (what an overwrite writes).
    pub content: String,
    /// The target as the host saw it when the card opened; `None` when it could not stat it.
    pub stamp: Option<FileStamp>,
}

/// Fingerprint both message role and full content, without depending on preview truncation.
#[must_use]
pub fn message_fingerprint(role_tag: &str, content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    role_tag.hash(&mut hasher);
    content.hash(&mut hasher);
    hasher.finish()
}

/// Use the same stable role tags at capture and effect time.
#[must_use]
pub fn message_target_fingerprint(
    message: &crate::domain::models::conversation::ChatMessage,
) -> u64 {
    use crate::domain::models::MessageRole;
    let role = match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
    };
    message_fingerprint(role, &message.content)
}

fn bounded(preview: &str) -> String {
    preview.chars().take(PREVIEW_MAX_CHARS).collect()
}

/// Raise the fork confirm of `message_index` (the user asked: Requested arrival).
pub fn raise_fork(
    state: &mut TuiState,
    conversation_id: &str,
    message_index: usize,
    preview: &str,
    target_hash: u64,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::Fork, "")
        .conversation(conversation_id)
        .source(message_index.to_string())
        .summary(format!("message {}", message_index + 1))
        .subject(DecisionSubject::Fork(Box::new(ForkPayload {
            conversation_id: conversation_id.to_owned(),
            message_index,
            target_hash,
            preview: bounded(preview),
        })));
    raise(state, draft, Arrival::Requested)
}

/// Raise the rewind confirm (the user asked: Requested arrival).
pub fn raise_rewind(
    state: &mut TuiState,
    conversation_id: &str,
    preview: RewindPreview,
    target_hash: u64,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::Rewind, "")
        .conversation(conversation_id)
        .source(preview.target_message_index.to_string())
        .summary(format!("message {}", preview.target_message_index + 1))
        .subject(DecisionSubject::Rewind(Box::new(RewindPayload {
            conversation_id: conversation_id.to_owned(),
            target_hash,
            preview,
        })));
    raise(state, draft, Arrival::Requested)
}

/// Raise the export overwrite confirm of `target` (the user asked: Requested arrival).
pub fn raise_export_overwrite(state: &mut TuiState, payload: ExportOverwritePayload) -> Raised {
    let name = payload
        .target
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let draft = DecisionDraft::new(DecisionKind::ExportOverwrite, "")
        .conversation(payload.conversation_id.clone())
        .source(payload.target.display().to_string())
        .summary(name)
        .subject(DecisionSubject::ExportOverwrite(Box::new(payload)));
    raise(state, draft, Arrival::Requested)
}

/// True while a fork of `conversation_id` waits.
#[must_use]
pub fn fork_pending(state: &TuiState, conversation_id: &str) -> bool {
    holds_in(state, DecisionKind::Fork, conversation_id)
}

/// True while a rewind of `conversation_id` waits.
#[must_use]
pub fn rewind_pending(state: &TuiState, conversation_id: &str) -> bool {
    holds_in(state, DecisionKind::Rewind, conversation_id)
}

/// Withdraw decisions targeting messages removed from this conversation.
pub fn retire_message_targets(state: &mut TuiState, conversation_id: &str, keep_len: usize) {
    let keys: Vec<_> = state
        .decisions
        .items()
        .filter(|item| {
            item.conversation_id.as_deref() == Some(conversation_id)
                && match &item.subject {
                    DecisionSubject::Fork(payload) => payload.message_index >= keep_len,
                    DecisionSubject::Rewind(payload) => {
                        payload.preview.target_message_index >= keep_len
                    }
                    _ => false,
                }
        })
        .map(DecisionItem::key)
        .collect();
    for key in keys {
        super::withdraw_key(state, &key, super::NO_LONGER_PENDING);
    }
}

/// True once the item's message target is out of reach of the conversation the facts
/// describe (its active one): a stale target never forks or rewinds (A7, K9).
#[must_use]
pub(super) fn target_gone(item: &DecisionItem, facts: &LogFacts) -> bool {
    let index = match &item.subject {
        DecisionSubject::Fork(payload) => payload.message_index,
        DecisionSubject::Rewind(payload) => payload.preview.target_message_index,
        _ => return false,
    };
    index >= facts.active_messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_6_fork_preview_is_bounded_and_the_key_names_the_message() {
        let mut state = TuiState::new(80, 24);
        let long = "x".repeat(PREVIEW_MAX_CHARS * 2);
        raise_fork(&mut state, "c1", 3, &long, 0);
        let item = state.decisions.items().next().expect("fork item");
        assert_eq!(item.source, "3");
        assert_eq!(item.conversation_id.as_deref(), Some("c1"));
        let DecisionSubject::Fork(payload) = &item.subject else {
            panic!("fork payload");
        };
        assert_eq!(payload.preview.chars().count(), PREVIEW_MAX_CHARS);
    }

    #[test]
    fn story21_6_target_gone_when_the_conversation_no_longer_reaches_the_message() {
        let mut state = TuiState::new(80, 24);
        raise_fork(&mut state, "c1", 3, "hello", 0);
        let item = state.decisions.items().next().expect("fork item").clone();
        let facts = |messages| LogFacts {
            active_conversation_id: Some("c1".into()),
            active_messages: messages,
            ..LogFacts::default()
        };
        assert!(!target_gone(&item, &facts(4)));
        assert!(target_gone(&item, &facts(3)));
    }

    /// 6D-L1-01: role and full content both bind the approved target.
    #[test]
    fn fingerprint_distinguishes_role_and_content() {
        let hash = message_fingerprint("user", "hello");
        assert_eq!(hash, message_fingerprint("user", "hello"));
        assert_ne!(hash, message_fingerprint("assistant", "hello"));
        assert_ne!(hash, message_fingerprint("user", "hello!"));
    }

    /// 6D-L1-01: truncation retires only removed targets of the affected conversation.
    #[test]
    fn retire_message_targets_is_scoped_and_marks_open_frame_stale() {
        let mut state = TuiState::new(80, 24);
        raise_fork(&mut state, "c1", 3, "removed", 1);
        raise_fork(&mut state, "c1", 0, "retained", 2);
        raise_fork(&mut state, "c2", 3, "other", 3);
        raise_rewind(
            &mut state,
            "c1",
            RewindPreview {
                target_message_index: 2,
                messages_to_remove: 1,
                files_to_revert: vec![],
            },
            4,
        );
        retire_message_targets(&mut state, "c1", 1);
        assert!(
            state
                .decisions
                .items()
                .any(|i| i.conversation_id.as_deref() == Some("c1")
                    && i.source == "0"
                    && !i.is_stale())
        );
        assert!(
            state
                .decisions
                .items()
                .any(|i| i.conversation_id.as_deref() == Some("c2")
                    && i.source == "3"
                    && !i.is_stale())
        );
        assert!(
            !state
                .decisions
                .items()
                .any(|i| i.conversation_id.as_deref() == Some("c1")
                    && i.kind == DecisionKind::Rewind
                    && !i.is_stale())
        );
        assert!(
            !state
                .decisions
                .items()
                .any(|i| i.conversation_id.as_deref() == Some("c1")
                    && i.source == "3"
                    && !i.is_stale())
        );
    }

    /// 6D-L1-01: an invalidated visible frame stays open only as a stale Close frame.
    #[test]
    fn retire_visible_target_marks_it_stale() {
        let mut state = TuiState::new(80, 24);
        super::super::prune_closed(
            &mut state,
            &LogFacts {
                active_conversation_id: Some("c1".into()),
                ..LogFacts::default()
            },
        );
        raise_fork(&mut state, "c1", 3, "removed", 1);
        retire_message_targets(&mut state, "c1", 1);
        let item = state
            .decisions
            .items()
            .find(|i| i.kind == DecisionKind::Fork)
            .expect("visible stale item");
        assert!(item.is_stale());
    }
}
