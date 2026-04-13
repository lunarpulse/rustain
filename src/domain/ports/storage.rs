#![allow(dead_code)]
use std::path::Path;

use async_trait::async_trait;

use crate::domain::errors::StorageError;
use crate::domain::models::checkpoint::{CheckpointId, CheckpointMeta, RevertedFile};
use crate::domain::models::{Conversation, ConversationSummary, SessionMeta};

/// Persistence for conversations and settings.
///
/// Claudian equivalent: `src/core/persistence/conversationPersistence.ts`
#[async_trait]
pub trait StoragePort: Send + Sync {
    async fn save_conversation(&self, conv: &Conversation) -> Result<(), StorageError>;
    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError>;
    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>, StorageError>;

    /// Delete a conversation and its metadata.
    /// Default implementation returns NotSupported error.
    async fn delete_conversation(&self, _id: &str) -> Result<(), StorageError> {
        Err(StorageError::NotSupported(
            "delete_conversation".to_string(),
        ))
    }

    /// Save session metadata sidecar file.
    /// Default implementation returns NotSupported error.
    async fn save_session_meta(&self, _id: &str, _meta: &SessionMeta) -> Result<(), StorageError> {
        Err(StorageError::NotSupported("save_session_meta".to_string()))
    }

    /// Load session metadata sidecar file.
    /// Default implementation returns NotSupported error.
    async fn load_session_meta(&self, _id: &str) -> Result<Option<SessionMeta>, StorageError> {
        Err(StorageError::NotSupported("load_session_meta".to_string()))
    }

    /// Fork conversation at a given checkpoint into a new conversation.
    /// Returns the new conversation's ID. Original is unchanged.
    /// Default implementation returns NotSupported error.
    async fn fork_at_checkpoint(
        &self,
        _source_conversation_id: &str,
        _checkpoint: CheckpointId,
    ) -> Result<String, StorageError> {
        Err(StorageError::NotSupported("fork".into()))
    }

    // ── Checkpoint Protocol (Amendment 1, Story 4-3b) ─────────────────────────────
    //
    // One checkpoint per tool-executing turn. `create_checkpoint` is called BEFORE
    // any tool in that turn runs, and `snapshot_file` is called once per file-writing
    // tool within that turn. `revert_to_checkpoint` truncates the conversation and
    // `revert_file_snapshots` restores files.
    //
    // All methods have default `NotSupported` implementations so that existing adapters
    // (NoOpStorageAdapter) compile unchanged. Only FileSystemStorage implements them.

    /// Create a checkpoint for the given conversation BEFORE tool dispatch.
    /// Returns a monotonically increasing `CheckpointId` persisted in `checkpoints.json`.
    ///
    /// Default: `Err(StorageError::NotSupported("checkpoints"))`.
    async fn create_checkpoint(
        &self,
        _conversation_id: &str,
    ) -> Result<CheckpointId, StorageError> {
        Err(StorageError::NotSupported("checkpoints".to_string()))
    }

    /// Return all checkpoints for a conversation, sorted by id ascending.
    ///
    /// Default: `Err(StorageError::NotSupported("list_checkpoints"))`.
    async fn list_checkpoints(
        &self,
        _conversation_id: &str,
    ) -> Result<Vec<CheckpointMeta>, StorageError> {
        Err(StorageError::NotSupported("list_checkpoints".to_string()))
    }

    /// Truncate the conversation to the messages at or before the given checkpoint.
    /// Also atomically updates `meta.json` and prunes the checkpoint log.
    ///
    /// Default: `Err(StorageError::NotSupported("revert"))`.
    async fn revert_to_checkpoint(
        &self,
        _conversation_id: &str,
        _checkpoint: CheckpointId,
    ) -> Result<Conversation, StorageError> {
        Err(StorageError::NotSupported("revert".to_string()))
    }

    /// Truncate the conversation to `conv.messages[0..=target_message_index]`,
    /// regardless of whether a checkpoint exists at that boundary.
    ///
    /// This is the message-level truncation primitive for rewind: unlike
    /// `revert_to_checkpoint`, it is keyed on the user's *selected message
    /// index* (from the chat pane navigation), not on a checkpoint id. This
    /// allows text-only conversations (no tool calls → no checkpoints) to be
    /// rewound, and prevents rewind from over-truncating to the nearest
    /// earlier checkpoint's message index.
    ///
    /// Also atomically updates `meta.json` (message_count, updated_at,
    /// preserving the DF-088 `extra` flatten map) and prunes the checkpoint
    /// log to entries with `message_index <= target_message_index`.
    ///
    /// File-snapshot reversal is a separate concern — callers must invoke
    /// `revert_file_snapshots` explicitly with the appropriate checkpoint
    /// floor.
    ///
    /// Default: `Err(StorageError::NotSupported("truncate_conversation"))`.
    async fn truncate_conversation(
        &self,
        _conversation_id: &str,
        _target_message_index: usize,
    ) -> Result<Conversation, StorageError> {
        Err(StorageError::NotSupported(
            "truncate_conversation".to_string(),
        ))
    }

    /// Snapshot the **original** content of `path` under the given checkpoint.
    /// The snapshot is written to `{session_dir}/snapshots/{cp_id}_{path_hash}`.
    /// Idempotent: if a snapshot already exists for this (checkpoint, path) pair,
    /// the first snapshot wins (preserves the pre-modification content).
    ///
    /// Default: `Err(StorageError::NotSupported("snapshot_file"))`.
    async fn snapshot_file(
        &self,
        _conversation_id: &str,
        _checkpoint: CheckpointId,
        _path: &Path,
        _content: &[u8],
    ) -> Result<(), StorageError> {
        Err(StorageError::NotSupported("snapshot_file".to_string()))
    }

    /// Revert all snapshots whose checkpoint id is strictly greater than `after_checkpoint`.
    /// Returns a `Vec<RevertedFile>` describing the outcome for each file.
    /// Snapshot files for `cp_id > after_checkpoint` are deleted after a successful revert
    /// to prevent re-application on a subsequent rewind.
    ///
    /// Default: `Err(StorageError::NotSupported("revert_file_snapshots"))`.
    async fn revert_file_snapshots(
        &self,
        _conversation_id: &str,
        _after_checkpoint: CheckpointId,
    ) -> Result<Vec<RevertedFile>, StorageError> {
        Err(StorageError::NotSupported(
            "revert_file_snapshots".to_string(),
        ))
    }

    /// Read-only preview: list files that would be reverted if `revert_file_snapshots`
    /// were called with `after_checkpoint`. Returns `(path, is_conflict)` pairs where
    /// `is_conflict` is true if the file's current content differs from the stored hash
    /// (same detection logic as `revert_file_snapshots`, but no files are modified).
    ///
    /// Default: `Ok(vec![])` — preview degrades gracefully when storage doesn't support it.
    async fn list_snapshot_files(
        &self,
        _conversation_id: &str,
        _after_checkpoint: CheckpointId,
    ) -> Result<Vec<(std::path::PathBuf, bool)>, StorageError> {
        Ok(vec![])
    }
}
