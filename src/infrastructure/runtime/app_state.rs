//! Runtime application state — session-level fields that span the event loop.

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::adapters::budget::BudgetStateStore;
use crate::adapters::provider::ProviderRegistry;
use crate::domain::models::{AppConfig, SandboxPolicy};
use crate::domain::ports::ConfigStorePort;
use crate::domain::ports::ProfileResolver;
use crate::domain::ports::{StreamingProvider, UsageLedgerPort};
use crate::domain::services::approval_runtime::ApprovalRuntime;
use crate::domain::services::plan_manager::PlanManager;
use crate::domain::services::plan_mode_injector::DefaultPlanInjector;
use crate::infrastructure::runtime::event_bus::EventBus;

/// Thin newtype implementing `ConfigStorePort` for the handler domain-isolation
/// contract (Story 8.1 AC-14). The handler takes `&dyn ConfigStorePort` instead
/// of `&AppState` so it doesn't import `crate::infrastructure::*`.
pub struct AppConfigStore {
    inner: Arc<ArcSwap<AppConfig>>,
}

impl ConfigStorePort for AppConfigStore {
    fn load(&self) -> Arc<AppConfig> {
        self.inner.load_full()
    }
    fn store(&self, config: AppConfig) {
        self.inner.store(Arc::new(config));
    }
}

pub struct AppState {
    pub session_cancel: CancellationToken,
    pub event_bus: Arc<EventBus>,
    pub approval_runtime: Arc<ApprovalRuntime>,
    pub sandbox_policy: Arc<RwLock<SandboxPolicy>>,
    pub plan_manager: Arc<PlanManager>,
    pub plan_injector: Arc<DefaultPlanInjector>,
    /// Active LLM provider wrapped for future hot-swap (Story 7.1b).
    pub provider: Arc<ArcSwap<Arc<dyn StreamingProvider>>>,
    /// Provider catalog
    pub provider_registry: Arc<ProviderRegistry>,
    /// Usage ledger for per-call token tracking (Story 7.1c).
    pub usage_ledger: Arc<dyn UsageLedgerPort>,
    /// Budget-pause persistence (Story 7.5 AC7).
    pub budget_state_store: Arc<BudgetStateStore>,
    /// Atomic config holder (Story 8.1 AC-7). Read with `.app_config.load()`.
    pub app_config: Arc<ArcSwap<AppConfig>>,
    /// Domain-pure config access for handlers (Story 8.1 AC-14).
    pub config_store: Arc<AppConfigStore>,
    /// Profile resolver (wrapped for hot-swap via Story 8.2 AC-15.2 / Story 8.4).
    pub profile_resolver: Arc<ArcSwap<Arc<dyn ProfileResolver>>>,
    /// CLI snapshot for config reload (Story 8.1 AC-10).
    pub cli_snapshot: crate::adapters::cli::commands::Cli,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        raw_capacity: usize,
        approval_runtime: Arc<ApprovalRuntime>,
        sandbox_policy: SandboxPolicy,
        plan_manager: Arc<PlanManager>,
        plan_injector: Arc<DefaultPlanInjector>,
        provider: Arc<ArcSwap<Arc<dyn StreamingProvider>>>,
        provider_registry: Arc<ProviderRegistry>,
        usage_ledger: Arc<dyn UsageLedgerPort>,
        budget_state_store: Arc<BudgetStateStore>,
        app_config: Arc<ArcSwap<AppConfig>>,
        profile_resolver: Arc<ArcSwap<Arc<dyn ProfileResolver>>>,
        cli_snapshot: crate::adapters::cli::commands::Cli,
    ) -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>,
    ) {
        let (event_bus, domain_rx) = EventBus::new(raw_capacity);
        let config_store = Arc::new(AppConfigStore {
            inner: app_config.clone(),
        });
        (
            Self {
                session_cancel: CancellationToken::new(),
                event_bus: Arc::new(event_bus),
                approval_runtime,
                sandbox_policy: Arc::new(RwLock::new(sandbox_policy)),
                plan_manager,
                plan_injector,
                provider,
                provider_registry,
                usage_ledger,
                budget_state_store,
                app_config,
                config_store,
                profile_resolver,
                cli_snapshot,
            },
            domain_rx,
        )
    }
}
