//! Provider router — active-provider dispatch with hot-swap support.
//!
//! The **router** is the dispatch surface: it holds multiple registered providers
//! and delegates `stream_completion()`, `abort()`, and `health_check()` to the
//! *active* provider. The active provider can be swapped at runtime via
//! `set_active()` (Story 7.2 model/provider switcher entry point).
//!
//! The **registry** (`ProviderRegistry`) is the catalog: it stores model metadata
//! and health status for all providers. Do not conflate the two — the registry is
//! for metadata queries; the router is for runtime dispatch.
//!
//! # Design notes
//!
//! - `routes` uses `ArcSwap<HashMap<...>>` for lock-free reads. Writes (register)
//!   clone the map, insert, and swap the Arc — rare and cheap.
//! - `active` uses `ArcSwap<String>` for the same reason.
//! - `stream_completion()` clones the `Arc<dyn StreamingProvider>` before `.await`
//!   so the load guard doesn't span the await point (per CLAUDE.md Async Lock Policy).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::domain::errors::ProviderError;
use crate::domain::models::{CompletionOptions, Message, ModelDescriptor, StreamChunk};
use crate::domain::ports::StreamingProvider;

/// Dispatches streaming completion calls to the currently-active provider.
///
/// Implements `StreamingProvider` itself so `AppState.provider` and `event_loop`
/// see no change — they continue consuming the `dyn StreamingProvider` contract (LSP).
pub struct ProviderRouter {
    routes: ArcSwap<HashMap<String, Arc<dyn StreamingProvider>>>,
    active: ArcSwap<String>,
}

impl ProviderRouter {
    /// Create a new router with the given initial active provider id.
    ///
    /// The active id MUST be registered via `register()` before any streaming
    /// calls, or `stream_completion()` will return an error.
    pub fn new(initial_active: String) -> Self {
        Self {
            routes: ArcSwap::from_pointee(HashMap::new()),
            active: ArcSwap::from_pointee(initial_active),
        }
    }

    /// Register a provider into the router.
    ///
    /// Takes `Arc<dyn StreamingProvider>` (not `Box<dyn …>`) to match the
    /// `AppState.provider` inner type. This asymmetry with `ProviderRegistry::register`
    /// is intentional — `ArcSwap<T>` requires `T: Sized`, so `Box<dyn T>` is
    /// unsized and cannot be held inside `ArcSwap` directly.
    pub fn register(&self, provider: Arc<dyn StreamingProvider>) {
        let id = provider.provider_id().to_string();
        let mut new_routes = HashMap::clone(&self.routes.load());
        new_routes.insert(id.clone(), provider);
        self.routes.store(Arc::new(new_routes));
        tracing::info!(target: "provider", "router registered: {}", id);
    }

    /// Set the active provider by id.
    ///
    /// Validates that the id exists in `routes`. On success, updates `active`
    /// and emits a `tracing::info!` log. This is the S7.2 swap surface.
    pub fn set_active(&self, provider_id: &str) -> Result<(), ProviderError> {
        let routes = self.routes.load();
        if !routes.contains_key(provider_id) {
            return Err(ProviderError::Other(format!(
                "router: unknown provider '{}'",
                provider_id
            )));
        }
        let prev = self.active.load().as_ref().clone();
        self.active.store(Arc::new(provider_id.to_string()));
        tracing::info!(
            target: "provider",
            "router active: {} -> {}",
            prev,
            provider_id
        );
        Ok(())
    }

    /// Return the active delegate's provider id (NOT the literal `"router"`).
    ///
    /// Used by the status bar to display `provider/model` instead of `router/model`.
    pub fn active_delegate_id(&self) -> String {
        self.active.load().as_ref().clone()
    }

    /// Lookup a registered provider by id.
    pub fn get_provider(&self, provider_id: &str) -> Option<Arc<dyn StreamingProvider>> {
        self.routes.load().get(provider_id).cloned()
    }
}

#[async_trait]
impl StreamingProvider for ProviderRouter {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let routes = self.routes.load();
        let active_id = self.active.load();
        let provider = routes.get(active_id.as_str()).cloned().ok_or_else(|| {
            ProviderError::Other(format!(
                "router: no active provider '{}'",
                active_id.as_str()
            ))
        })?;
        // Drop guards before await
        drop(routes);
        drop(active_id);
        provider.stream_completion(messages, options).await
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        let routes = self.routes.load();
        let active_id = self.active.load();
        let provider = routes.get(active_id.as_str()).cloned().ok_or_else(|| {
            ProviderError::Other(format!(
                "router: no active provider '{}'",
                active_id.as_str()
            ))
        })?;
        drop(routes);
        drop(active_id);
        provider.abort().await
    }

    fn provider_id(&self) -> String {
        "router".to_string()
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        let routes = self.routes.load();
        let mut models = Vec::new();
        for provider in routes.values() {
            models.extend(provider.list_models());
        }
        models
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        let routes = self.routes.load();
        let active_id = self.active.load();
        let provider = routes.get(active_id.as_str()).cloned().ok_or_else(|| {
            ProviderError::Other(format!(
                "router: no active provider '{}'",
                active_id.as_str()
            ))
        })?;
        drop(routes);
        drop(active_id);
        provider.health_check().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{Message, MessageRole, UserMessage};

    #[test]
    fn test_router_provider_id_returns_router_literal() {
        let router = ProviderRouter::new("noop".to_string());
        assert_eq!(router.provider_id(), "router");
    }

    #[test]
    fn test_router_set_active_unknown_returns_error() {
        let router = ProviderRouter::new("noop".to_string());
        let result = router.set_active("nope");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown provider 'nope'"));
    }

    #[test]
    fn test_router_active_delegate_id() {
        let router = ProviderRouter::new("anthropic".to_string());
        assert_eq!(router.active_delegate_id(), "anthropic");
        router.register(Arc::new(crate::adapters::noop::NoOpProvider));
        router.set_active("noop").unwrap();
        assert_eq!(router.active_delegate_id(), "noop");
    }
}
