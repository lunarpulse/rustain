#![allow(dead_code)]
use std::path::Path;

use async_trait::async_trait;

use crate::domain::errors::StorageError;
use crate::domain::models::checkpoint::{CheckpointId, CheckpointMeta, RevertedFile};
use crate::domain::models::transaction::{RewindTxn, RewindTxnPhase};
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

    /// Finalize a snapshot after the Write/Edit tool completes (DF-111, AC5, schema v3).
    ///
    /// Records `expected_current_hash` (hash of post-write content) in the snapshot
    /// envelope. At revert time, this distinguishes "tool-modified" (→ Restore) from
    /// "externally-modified since tool write" (→ Conflict).
    ///
    /// Called by the toolset adapter after each successful Write/Edit operation.
    /// Default: `Ok(())` — no-op (degrades gracefully when storage doesn't support it).
    async fn finalize_snapshot(
        &self,
        _conversation_id: &str,
        _checkpoint: CheckpointId,
        _path: &Path,
        _post_write_content: &[u8],
    ) -> Result<(), StorageError> {
        Ok(()) // no-op is safe — degrades to v2 semantics
    }

    // ── Rewind Transaction Journal (DF-109, AC3) ─────────────────────────────
    //
    // A transaction journal is written atomically before each phase of a rewind
    // so that a crash mid-operation can be detected and completed or aborted on
    // next startup.  All methods have default `NotSupported` or no-op
    // implementations so that `NoOpStorageAdapter` compiles unchanged.

    /// Write the initial `Pending` transaction journal for a rewind operation.
    ///
    /// Must be called BEFORE `truncate_conversation` or `revert_file_snapshots`.
    ///
    /// Default: `Err(StorageError::NotSupported("begin_rewind_txn"))`.
    async fn begin_rewind_txn(
        &self,
        _conversation_id: &str,
        _target_message_index: usize,
        _after_checkpoint: CheckpointId,
    ) -> Result<(), StorageError> {
        Err(StorageError::NotSupported("begin_rewind_txn".to_string()))
    }

    /// Advance the transaction journal to the given phase.
    ///
    /// Called after each phase completes (e.g., advance to `MessagesTruncated`
    /// after `truncate_conversation` returns `Ok`).
    ///
    /// Default: `Err(StorageError::NotSupported("write_rewind_phase"))`.
    async fn write_rewind_phase(
        &self,
        _conversation_id: &str,
        _phase: RewindTxnPhase,
    ) -> Result<(), StorageError> {
        Err(StorageError::NotSupported("write_rewind_phase".to_string()))
    }

    /// Mark the transaction committed and remove the journal file.
    ///
    /// Default: `Err(StorageError::NotSupported("commit_rewind_txn"))`.
    async fn commit_rewind_txn(&self, _conversation_id: &str) -> Result<(), StorageError> {
        Err(StorageError::NotSupported("commit_rewind_txn".to_string()))
    }

    /// Load the active transaction journal for a conversation, if any.
    ///
    /// Returns `None` if no journal exists (normal state).
    ///
    /// Default: `Ok(None)`.
    async fn load_rewind_txn(
        &self,
        _conversation_id: &str,
    ) -> Result<Option<RewindTxn>, StorageError> {
        Ok(None)
    }

    /// Scan all conversations for incomplete rewind journals and either complete
    /// or abort them.  Called once at storage adapter initialisation.
    ///
    /// - `Pending` journal → no operations started → delete journal (no-op).
    /// - `MessagesTruncated` journal → truncation completed but files not reverted
    ///   → run `revert_file_snapshots`, then delete journal.
    /// - `FilesReverted` journal → files reverted but messages not truncated
    ///   → run `truncate_conversation`, then delete journal.
    /// - `Committed` journal → already finished → delete stale journal.
    ///
    /// Logs results at `INFO` level.  Best-effort: individual failures are logged
    /// and skipped rather than aborting the entire reconciliation sweep.
    ///
    /// Default: `Ok(())` — no-op (degrades gracefully).
    async fn reconcile_pending_txns(&self) -> Result<(), StorageError> {
        Ok(())
    }
}
