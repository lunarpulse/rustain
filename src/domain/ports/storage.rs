#![allow(dead_code)]
use async_trait::async_trait;

use crate::domain::errors::StorageError;
use crate::domain::models::{Conversation, ConversationSummary};

/// Persistence for conversations and settings.
///
/// Claudian equivalent: `src/core/persistence/conversationPersistence.ts`
#[async_trait]
pub trait StoragePort: Send + Sync {
    async fn save_conversation(&self, conv: &Conversation) -> Result<(), StorageError>;
    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError>;
    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>, StorageError>;

    // v0.5+: async fn delete_conversation(&self, id: &str) -> Result<(), StorageError> { unimplemented!() }
    // v0.5+: async fn save_settings(&self, settings: &Settings) -> Result<(), StorageError> { unimplemented!() }
    // v0.5+: async fn load_settings(&self) -> Result<Settings, StorageError> { unimplemented!() }
    // v0.5+: async fn search_conversations(&self, query: &str) -> Result<Vec<SearchResult>, StorageError> { unimplemented!() }
}
