//! Fork/rewind storage actions use durable transcript coordinates, not live rows.

use crate::domain::errors::StorageError;
use crate::domain::models::{Conversation, RevertedFile};
use crate::domain::ports::{RewindStep, StoragePort};

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
    // The flush above is the accepted predecessor: the fork projects exactly that version
    // (or is refused before any write when another writer was accepted after it).
    storage.fork_conversation(conversation, index).await
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

/// Rewind at a live row. Like the fork, the live durable transcript is written first: a
/// durable row still only in memory (the plan summary) is otherwise past the stored end and
/// rewind would drop it. The caller's in-flight background save is completed before the
/// flush so a stale clone cannot shorten the file again; a failed flush starts nothing.
/// The store then owns journal, file reversion, truncation and installation as one operation
/// bound to the flushed version.
pub async fn handle_rewind_confirm(
    storage: &dyn StoragePort,
    pending_save: &mut Option<tokio::task::JoinHandle<()>>,
    conversation: &Conversation,
    live_index: usize,
) -> Result<RewindOutcome, RewindFailure> {
    let index = durable_target(conversation, live_index).map_err(RewindFailure::Begin)?;
    if let Some(previous) = pending_save.take() {
        let _ = previous.await;
    }
    storage
        .save_conversation(conversation)
        .await
        .map_err(RewindFailure::Begin)?;
    let rewound = storage
        .rewind_conversation(conversation, index)
        .await
        .map_err(|step| match step {
            RewindStep::Begin(error) => RewindFailure::Begin(error),
            RewindStep::Truncate(error) => RewindFailure::Truncate(error),
        })?;
    Ok(RewindOutcome {
        conversation: rewound.conversation,
        reverted: rewound.reverted,
    })
}
