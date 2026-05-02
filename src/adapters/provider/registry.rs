//! Provider registry — manages the set of registered LLM providers.
//!
//! The registry owns the provider instances and provides model metadata queries
//! without crossing the port boundary (AC6). The active provider is selected
//! by `AppState.provider` (ArcSwap) — this registry is the catalog, not the router.
//!
//! # Public API
//!
//! - `register(provider)` — add a provider to the registry
//! - `get_model(provider_id, model_id)` — lookup a single model descriptor
//! - `list_models_by_provider(provider_id)` — models for one provider
//! - `list_all_models()` — all models across all providers
//! - `list_providers()` — provider-level descriptors for UI

use std::collections::HashMap;
use std::sync::RwLock; // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: ProviderRegistry methods are sync, short critical sections, never across .await

use crate::domain::models::provider::ModelDescriptor;
use crate::domain::models::ProviderDescriptor;
use crate::domain::ports::StreamingProvider;

pub struct ProviderRegistry {
    providers: RwLock<HashMap<String, Box<dyn StreamingProvider>>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
        }
    }

    /// Register a provider. If a provider with the same ID already exists,
    /// it is replaced.
    pub fn register(&self, provider: Box<dyn StreamingProvider>) {
        let id = provider.provider_id().to_string();
        self.providers.write().unwrap().insert(id, provider);
    }

    /// Lookup a single model by (provider_id, model_id).
    pub fn get_model(&self, provider_id: &str, model_id: &str) -> Option<ModelDescriptor> {
        let providers = self.providers.read().unwrap();
        let provider = providers.get(provider_id)?;
        provider
            .list_models()
            .into_iter()
            .find(|m| m.model_id == model_id)
    }

    /// List all models for a specific provider.
    pub fn list_models_by_provider(&self, provider_id: &str) -> Vec<ModelDescriptor> {
        let providers = self.providers.read().unwrap();
        match providers.get(provider_id) {
            Some(p) => p.list_models(),
            None => vec![],
        }
    }

    /// List all models from all registered providers.
    pub fn list_all_models(&self) -> Vec<ModelDescriptor> {
        let providers = self.providers.read().unwrap();
        let mut models = Vec::new();
        for (_id, provider) in providers.iter() {
            models.extend(provider.list_models());
        }
        models
    }

    /// Return provider-level descriptors for UI display.
    pub fn list_providers(&self) -> Vec<ProviderDescriptor> {
        let providers = self.providers.read().unwrap();
        providers
            .values()
            .map(|p| {
                let models = p.list_models();
                ProviderDescriptor {
                    provider_id: p.provider_id().to_string(),
                    healthy: true, // health_check is run at startup, not here
                    model_count: models.len(),
                    display_name: p.provider_id().to_string(),
                }
            })
            .collect()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
