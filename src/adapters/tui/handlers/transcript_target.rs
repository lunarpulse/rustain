//! Fork/rewind storage actions use durable transcript coordinates, not live rows.

use crate::domain::errors::StorageError;
use crate::domain::models::checkpoint::CheckpointId;
use crate::domain::models::{Conversation, RevertedFile};
use crate::domain::ports::StoragePort;

/// Refuse an answer whose index now names a different message.
pub fn verify_target(
    conversation: &Conversation,
    index: usize,
    target_hash: u64,
) -> Result<(), &'static str> {
    match conversation.messages.get(index) {
        Some(message)
            if crate::adapters::tui::decision_queue::message_target_fingerprint(message)
                == target_hash =>
        {
            Ok(())
        }
        _ => Err("This decision is no longer pending."),
    }
}

pub fn durable_target(
    conversation: &Conversation,
    live_index: usize,
) -> Result<usize, StorageError> {
    conversation
        .durable_message_index(live_index)
        .ok_or_else(|| {
            StorageError::Other(
                "Temporary display rows are not persistent fork/rewind targets".into(),
            )
        })
}

/// Fork at a live row. The live durable transcript is written first so the storage
/// re-read in `fork_at_checkpoint` addresses the rows the operator chose: a durable row
/// still only in memory (the plan summary) is otherwise past the stored end. The caller
/// passes the event loop's in-flight background save, which is completed before the
/// flush so a stale clone cannot shorten the file again, and whether a turn producer of
/// this conversation is still live: a running turn saves the same source on its own, so
/// the fork is refused until it ends. A failed flush aborts the fork.
pub async fn handle_fork_confirm(
    storage: &dyn StoragePort,
    pending_save: &mut Option<tokio::task::JoinHandle<()>>,
    producer_live: bool,
    conversation: &Conversation,
    live_index: usize,
) -> Result<String, StorageError> {
    if producer_live {
        return Err(StorageError::Other(
            "A response is still running. Wait for it to finish, then fork again.".into(),
        ));
    }
    let index = durable_target(conversation, live_index)?;
    if let Some(previous) = pending_save.take() {
        let _ = previous.await;
    }
    storage.save_conversation(conversation).await?;
    storage
        .fork_at_checkpoint(&conversation.id, CheckpointId(index as u64))
        .await
}

pub async fn handle_rewind_fork_instead(
    storage: &dyn StoragePort,
    pending_save: &mut Option<tokio::task::JoinHandle<()>>,
    producer_live: bool,
    conversation: &Conversation,
    index: usize,
) -> Result<String, StorageError> {
    handle_fork_confirm(storage, pending_save, producer_live, conversation, index).await
}

pub struct RewindOutcome {
    pub conversation: Conversation,
    pub reverted: Vec<RevertedFile>,
}

#[derive(Debug)]
pub enum RewindFailure {
    Begin(StorageError),
    Truncate(StorageError),
}

pub async fn handle_rewind_confirm(
    storage: &dyn StoragePort,
    conversation: &Conversation,
    live_index: usize,
) -> Result<RewindOutcome, RewindFailure> {
    let index = durable_target(conversation, live_index).map_err(RewindFailure::Begin)?;
    storage
        .begin_rewind_txn(&conversation.id, index)
        .await
        .map_err(RewindFailure::Begin)?;
    // Preserve the existing best-effort file-reversion policy and journal order:
    // revert before truncation prunes the checkpoint log.
    let reverted = storage
        .revert_file_snapshots(&conversation.id, index)
        .await
        .unwrap_or_default();
    let truncated = storage
        .truncate_conversation(&conversation.id, index)
        .await
        .map_err(RewindFailure::Truncate)?;
    if let Err(error) = storage.commit_rewind_txn(&conversation.id).await {
        tracing::warn!("commit_rewind_txn failed (non-fatal): {}", error);
    }
    Ok(RewindOutcome {
        conversation: truncated,
        reverted,
    })
}
