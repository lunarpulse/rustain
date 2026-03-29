#![allow(dead_code)]
use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::domain::errors::ProviderError;
use crate::domain::models::{CompletionOptions, Message, StreamChunk};

/// Streams LLM completions. Infrastructure orchestrates multi-turn tool loops.
///
/// Claudian equivalent: `src/core/providers/anthropic.ts`
#[async_trait]
pub trait ProviderPort: Send + Sync {
    /// Stream a single completion. Infrastructure orchestrates multi-turn tool loops.
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError>;

    /// Abort the current streaming completion.
    async fn abort(&self) -> Result<(), ProviderError>;

    /// Provider identifier (e.g., "anthropic", "openai", "ollama").
    fn provider_id(&self) -> &str;

    // v0.5+: async fn available_models(&self) -> Result<Vec<ModelInfo>, ProviderError> { ... }
    // v0.5+: async fn health_check(&self) -> Result<(), ProviderError> { ... }
}
