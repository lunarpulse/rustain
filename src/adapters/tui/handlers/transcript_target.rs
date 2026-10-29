//! Fork/rewind storage actions use durable transcript coordinates, not live rows.

use crate::adapters::tui::state::TuiState;
use crate::domain::errors::StorageError;
use crate::domain::models::checkpoint::CheckpointId;
use crate::domain::models::{Conversation, RevertedFile};
use crate::domain::ports::StoragePort;

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

pub async fn handle_fork_confirm(
    storage: &dyn StoragePort,
    conversation: &Conversation,
    live_index: usize,
) -> Result<String, StorageError> {
    let index = durable_target(conversation, live_index)?;
    storage
        .fork_at_checkpoint(&conversation.id, CheckpointId(index as u64))
        .await
}

pub async fn handle_rewind_fork_instead(
    storage: &dyn StoragePort,
    conversation: &Conversation,
    state: &mut TuiState,
) -> Option<(usize, Result<String, StorageError>)> {
    state.rewind_preview = None;
    let index = state.pending_rewind_index.take()?;
    Some((
        index,
        handle_fork_confirm(storage, conversation, index).await,
    ))
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
