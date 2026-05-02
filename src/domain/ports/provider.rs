#![allow(dead_code)]
use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
use crate::domain::models::{CompletionOptions, Message, StreamChunk};

/// Single port for LLM streaming completions.
///
/// Every provider (Anthropic, OpenAI, Ollama, ...) implements this trait.
/// Adding a new provider requires one trait implementation — no changes to
/// domain or other adapters (OCP). The `ProviderRouter` in Story 7.1b also
/// implements this trait, delegating to the active provider (ISP / decorator).
///
/// # Extension guide
///
/// To add a new provider:
/// 1. Implement `StreamingProvider` in `src/adapters/<name>/mod.rs`.
/// 2. Register the adapter in `src/main.rs`'s `ProviderRegistry`.
/// 3. Add provider config under `[provider.<name>]` in `rustain.toml`.
/// 4. No changes to `AgentCore`, `event_loop`, or domain models needed.
///
/// # v0.5+ methods
///
/// `health_check()`, `list_models()`, `provider_descriptor()` are populated
/// for Anthropic, OpenAI — all providers since S7.1a.
#[async_trait]
pub trait StreamingProvider: Send + Sync {
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

    /// Return the metadata of all models available through this provider.
    fn list_models(&self) -> Vec<ModelDescriptor>;

    /// Lightweight health check. Returns `Ok(())` if the provider API is
    /// reachable, `Err(ProviderError)` on timeout or auth failure.
    /// Must not block for more than 5 seconds.
    async fn health_check(&self) -> Result<(), ProviderError>;

    /// Return a provider-level descriptor for UI display.
    fn provider_descriptor(&self) -> ProviderDescriptor {
        let models = self.list_models();
        ProviderDescriptor {
            provider_id: self.provider_id().to_string(),
            healthy: true, // caller updates after health_check
            model_count: models.len(),
            display_name: self.provider_id().to_string(),
        }
    }
}
