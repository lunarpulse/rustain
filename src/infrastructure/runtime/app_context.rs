//! `AppContext` — borrowing wrapper that implements `ProviderInfoPort` over
//! the infrastructure-side `AppState` + adapter-side `ProviderRouter`.
//!
//! Established Story 8.0a Phase 2 (Task 3) per user direction
//! "per spec + long-term correctness". Lets domain-isolated handler modules
//! under `src/adapters/tui/handlers/` consume provider/router state through
//! a domain trait without importing infrastructure or adapter types.
//!
//! **Lifetime model:** `AppContext` is constructed per-dispatch-arm by
//! `event_loop.rs::run()` and passed as `&dyn ProviderInfoPort` to handlers.
//! It does NOT outlive the dispatch arm; the trait carries `Send + Sync` so
//! the wrapper composes with payloads passed to `tokio::spawn` (the spawned
//! task receives owned data extracted from the port, not the port itself).

use std::sync::Arc;

use crate::adapters::provider::ProviderRouter;
use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
use crate::domain::ports::{ProviderInfoPort, StreamingProvider};
use crate::infrastructure::runtime::app_state::AppState;

pub struct AppContext<'a> {
    pub app_state: &'a AppState,
    pub router: &'a ProviderRouter,
}

impl<'a> AppContext<'a> {
    pub fn new(app_state: &'a AppState, router: &'a ProviderRouter) -> Self {
        Self { app_state, router }
    }
}

impl<'a> ProviderInfoPort for AppContext<'a> {
    fn active_delegate_id(&self) -> String {
        self.router.active_delegate_id()
    }

    fn get_model(&self, provider_id: &str, model_id: &str) -> Option<ModelDescriptor> {
        self.app_state
            .provider_registry
            .get_model(provider_id, model_id)
    }

    fn get_model_provider(&self, model_id: &str, prefer: Option<&str>) -> Option<String> {
        self.app_state
            .provider_registry
            .get_model_provider(model_id, prefer)
    }

    fn list_providers(&self) -> Vec<ProviderDescriptor> {
        self.app_state.provider_registry.list_providers()
    }

    fn list_models_by_provider(&self, provider_id: &str) -> Vec<ModelDescriptor> {
        self.app_state
            .provider_registry
            .list_models_by_provider(provider_id)
    }

    fn get_provider(&self, provider_id: &str) -> Option<Arc<dyn StreamingProvider>> {
        self.router.get_provider(provider_id)
    }

    fn set_active_provider(&self, provider_id: &str) -> Result<(), ProviderError> {
        self.router.set_active(provider_id)
    }

    fn now_unix(&self) -> i64 {
        crate::infrastructure::clock_util::now_unix()
    }

    fn today_start_unix_ms(&self) -> i64 {
        crate::infrastructure::clock_util::today_start_unix_ms()
    }
}
