//! Runtime application state — session-level fields that span the event loop.

use std::sync::Arc;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::domain::models::SandboxPolicy;
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
}

impl AppState {
    pub fn new(
        raw_capacity: usize,
        approval_runtime: Arc<ApprovalRuntime>,
        sandbox_policy: SandboxPolicy,
        plan_manager: Arc<PlanManager>,
        plan_injector: Arc<DefaultPlanInjector>,
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
            },
            domain_rx,
        )
    }
}
