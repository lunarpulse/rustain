#![allow(dead_code)]
use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
use crate::domain::models::{CompletionOptions, Message, StreamChunk};

/// Outcome of a non-billable connectivity probe (Story 13.2 AC8).
/// Proves auth + reachability only — NOT that streaming/messages works.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    /// Round-trip latency of the probe request.
    pub latency: std::time::Duration,
}

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
    fn provider_id(&self) -> String;

    /// Return the metadata of all models available through this provider.
    fn list_models(&self) -> Vec<ModelDescriptor>;

    /// Lightweight health check. Returns `Ok(())` if the provider API is
    /// reachable, `Err(ProviderError)` on timeout or auth failure.
    /// Must not block for more than 5 seconds.
    async fn health_check(&self) -> Result<(), ProviderError>;

    /// Non-billable connectivity probe (Story 13.2 AC8).
    ///
    /// Uses a free, idempotent endpoint (`GET /v1/models` for Anthropic/OpenAI,
    /// `GET /api/tags` for Ollama) to validate auth + reachability WITHOUT
    /// costing a token or writing to provider logs.
    ///
    /// Returns `Ok(ProbeOutcome)` with latency on success.
    /// - 401/403 → `Err(ProviderError::AuthenticationFailed)`
    /// - Transport/connect/timeout → `Err(ProviderError::Offline(…))`
    /// - 404/405 → `Err(ProviderError::Other(…))` (endpoint unsupported)
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError>;

    /// Return a provider-level descriptor for UI display.
    fn provider_descriptor(&self) -> ProviderDescriptor {
        let models = self.list_models();
        let pid = self.provider_id();
        ProviderDescriptor {
            provider_id: pid.clone(),
            healthy: true, // caller updates after health_check
            model_count: models.len(),
            display_name: pid,
        }
    }
}
