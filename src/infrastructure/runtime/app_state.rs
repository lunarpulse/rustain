//! Runtime application state — session-level fields that span the event loop.

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::adapters::provider::ProviderRegistry;
use crate::domain::models::SandboxPolicy;
use crate::domain::ports::StreamingProvider;
use crate::domain::services::approval_runtime::ApprovalRuntime;
use crate::domain::services::plan_manager::PlanManager;
use crate::domain::services::plan_mode_injector::DefaultPlanInjector;
use crate::infrastructure::runtime::event_bus::EventBus;

pub struct AppState {
    pub session_cancel: CancellationToken,
    pub event_bus: Arc<EventBus>,
    pub approval_runtime: Arc<ApprovalRuntime>,
    pub sandbox_policy: Arc<RwLock<SandboxPolicy>>,
    pub plan_manager: Arc<PlanManager>,
    pub plan_injector: Arc<DefaultPlanInjector>,
    /// Active LLM provider wrapped for future hot-swap (Story 7.1b).
    /// Load with `.load()` → `Arc<dyn StreamingProvider>`.
    // ProviderRouter added in Story 7.1b.
    pub provider: Arc<ArcSwap<Arc<dyn StreamingProvider>>>,
    /// Provider catalog — model metadata queries without crossing port boundary (AC6).
    pub provider_registry: Arc<ProviderRegistry>,
}

impl AppState {
    pub fn new(
        raw_capacity: usize,
        approval_runtime: Arc<ApprovalRuntime>,
        sandbox_policy: SandboxPolicy,
        plan_manager: Arc<PlanManager>,
        plan_injector: Arc<DefaultPlanInjector>,
        provider: Arc<ArcSwap<Arc<dyn StreamingProvider>>>,
        provider_registry: Arc<ProviderRegistry>,
    ) -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>,
    ) {
        let (event_bus, domain_rx) = EventBus::new(raw_capacity);
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
            },
            domain_rx,
        )
    }
}
