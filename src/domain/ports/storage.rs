#![allow(dead_code)]
use async_trait::async_trait;

use crate::domain::errors::StorageError;
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
        Err(StorageError::NotSupported("delete_conversation".to_string()))
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
}
