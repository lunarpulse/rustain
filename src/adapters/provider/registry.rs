//! Provider registry — manages the set of registered LLM providers.
//!
//! The registry owns the provider instances and provides model metadata queries
//! without crossing the port boundary (AC6). The active provider is selected
//! by `AppState.provider` (ArcSwap) — this registry is the catalog, not the router.
//!
//! # Public API
//!
//! - `register(provider)` — add a Box<dyn StreamingProvider> to the registry
//! - `register_arc(provider)` — add an Arc<dyn StreamingProvider> to the registry
//! - `get_model(provider_id, model_id)` — lookup a single model descriptor
//! - `list_models_by_provider(provider_id)` — models for one provider
//! - `list_models_by_capability(capability)` — models supporting a capability
//! - `list_all_models()` — all models across all providers
//! - `list_providers()` — provider-level descriptors for UI
//! - `update_health(provider_id, healthy)` — record health-check result

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::RwLock; // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: ProviderRegistry methods are sync, short critical sections, never across .await

use crate::domain::models::ProviderDescriptor;
use crate::domain::models::provider::{ModelCapability, ModelDescriptor};
use crate::domain::ports::StreamingProvider;

pub struct ProviderRegistry {
    providers: RwLock<HashMap<String, Arc<dyn StreamingProvider>>>,
    health_status: RwLock<HashMap<String, bool>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
            health_status: RwLock::new(HashMap::new()),
        }
    }

    /// Register a boxed provider. If a provider with the same ID already exists,
    /// it is replaced. The provider starts with healthy=true until health-check runs.
    pub fn register(&self, provider: Box<dyn StreamingProvider>) {
        let id = provider.provider_id();
        self.providers
            .write()
            .expect("ProviderRegistry lock poisoned")
            .insert(id.clone(), Arc::from(provider));
        self.health_status
            .write()
            .expect("ProviderRegistry health lock poisoned")
            .insert(id, true);
    }

    /// Register an Arc-wrapped provider (avoids double-allocation when caller
    /// already holds an Arc, e.g. the active provider on AppState).
    pub fn register_arc(&self, provider: Arc<dyn StreamingProvider>) {
        let id = provider.provider_id();
        self.providers
            .write()
            .expect("ProviderRegistry lock poisoned")
            .insert(id.clone(), provider);
        self.health_status
            .write()
            .expect("ProviderRegistry health lock poisoned")
            .insert(id, true);
    }

    /// Record the health-check result for a provider.
    pub fn update_health(&self, provider_id: &str, healthy: bool) {
        self.health_status
            .write()
            .expect("ProviderRegistry health lock poisoned")
            .insert(provider_id.to_string(), healthy);
    }

    /// Lookup a single model by (provider_id, model_id).
    pub fn get_model(&self, provider_id: &str, model_id: &str) -> Option<ModelDescriptor> {
        let providers = self
            .providers
            .read()
            .expect("ProviderRegistry lock poisoned");
        let provider = providers.get(provider_id)?;
        provider
            .list_models()
            .into_iter()
            .find(|m| m.model_id == model_id)
    }

    /// List all models for a specific provider.
    pub fn list_models_by_provider(&self, provider_id: &str) -> Vec<ModelDescriptor> {
        let providers = self
            .providers
            .read()
            .expect("ProviderRegistry lock poisoned");
        match providers.get(provider_id) {
            Some(p) => p.list_models(),
            None => vec![],
        }
    }

    /// List all models that support a given capability (AC2: queryable by capability).
    pub fn list_models_by_capability(&self, capability: &ModelCapability) -> Vec<ModelDescriptor> {
        let providers = self
            .providers
            .read()
            .expect("ProviderRegistry lock poisoned");
        let mut result = Vec::new();
        for (_id, provider) in providers.iter() {
            for model in provider.list_models() {
                if model.capabilities.contains(capability) {
                    result.push(model);
                }
            }
        }
        result
    }

    /// List all models from all registered providers.
    pub fn list_all_models(&self) -> Vec<ModelDescriptor> {
        let providers = self
            .providers
            .read()
            .expect("ProviderRegistry lock poisoned");
        let mut models = Vec::new();
        for (_id, provider) in providers.iter() {
            models.extend(provider.list_models());
        }
        models
    }

    /// Return provider-level descriptors for UI display.
    /// Uses stored health status from `update_health()`.
    pub fn list_providers(&self) -> Vec<ProviderDescriptor> {
        let providers = self
            .providers
            .read()
            .expect("ProviderRegistry lock poisoned");
        let health = self
            .health_status
            .read()
            .expect("ProviderRegistry health lock poisoned");
        providers
            .values()
            .map(|p| {
                let pid = p.provider_id();
                let models = p.list_models();
                ProviderDescriptor {
                    provider_id: pid.clone(),
                    healthy: *health.get(&pid).unwrap_or(&true),
                    model_count: models.len(),
                    display_name: pid,
                }
            })
            .collect()
    }

    /// Return the set of provider IDs currently registered.
    pub fn provider_ids(&self) -> HashSet<String> {
        let providers = self
            .providers
            .read()
            .expect("ProviderRegistry lock poisoned");
        providers.keys().cloned().collect()
    }

    /// Resolve the provider_id that owns a given model_id (Story 7.2 AC6).
    /// Scans all providers' model lists; returns the first match.
    pub fn get_model_provider(&self, model_id: &str) -> Option<String> {
        let providers = self
            .providers
            .read()
            .expect("ProviderRegistry lock poisoned");
        for (_id, provider) in providers.iter() {
            if provider
                .list_models()
                .iter()
                .any(|m| m.model_id == model_id)
            {
                return Some(provider.provider_id());
            }
        }
        None
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
