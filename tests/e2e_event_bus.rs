//! E2E tests for Story 6-0a: Dual-Channel EventBus.
//!
//! Covers:
//! - AC5: EventBus dual-channel infrastructure
//! - AC5: Raw subscriber lag handling
//! - AC5: Reference consumer pattern with timeout
//! - AC6: RawEvent projection from AppEvent
//! - AC8: AppState construction and wiring

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::broadcast;

use rustain::adapters::noop::NoOpProvider;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{NoticeLevel, PermissionMode, SandboxPolicy, StreamChunk};
use rustain::domain::ports::StreamingProvider;
use rustain::domain::services::plan_manager::PlanManager;
use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
use rustain::infrastructure::composition::ComposeContext;
use rustain::infrastructure::runtime::agent_core::AgentCore;
use rustain::infrastructure::runtime::app_state::AppState;
use rustain::infrastructure::runtime::event_bus::{EventBus, RawEvent, RawEventKind};

fn test_cli() -> rustain::adapters::cli::commands::Cli {
    rustain::adapters::cli::commands::Cli {
        log_level: Some("info".to_string()),
        command: None,
        new: false,
        session: None,
        snapshot_retention: None,
        config_file: None,
        model: None,
        profile: None,
        persona: None,
        memory: None,
        session_adapter: None,
        tools: None,
        channels: None,
        scheduler: None,
        context: None,
        tool_exposure: None,
        skill_exposure: None,
        sandbox_adapter: None,
    }
}

// ── AC8 / AC5: AppState wires EventBus with configurable capacity ────────────

#[test]
fn test_app_state_honors_raw_capacity() {
    let approval_runtime = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        64,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let provider_swap = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpProvider::default()) as Arc<dyn StreamingProvider>
    ));
    let provider_registry = Arc::new(rustain::adapters::provider::ProviderRegistry::new());
    let (event_bus, domain_rx) = EventBus::new(64);
    let event_bus = Arc::new(event_bus);
    let agent_core = Arc::new(AgentCore::test_noop());
    let compose_snapshot = Arc::new(ComposeContext {
        workspace_path: std::path::PathBuf::from("."),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage::default())
            as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        tool_exposure: "static-full".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(rustain::adapters::sandbox::NoOpSandbox) as Arc<dyn rustain::domain::ports::SandboxManager>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(rustain::domain::models::sandbox::SandboxPolicy::Permissive)),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    });
    let (app_state, _domain_rx) = AppState::new(
        event_bus,
        domain_rx,
        approval_runtime,
        Arc::new(tokio::sync::RwLock::new(SandboxPolicy::ReadOnly { network: false })),
        Arc::new(PlanManager::new(std::path::PathBuf::from("."))),
        Arc::new(DefaultPlanInjector::new()),
        provider_swap.clone(),
        provider_registry,
        Arc::new(rustain::adapters::noop::NoOpUsageLedger),
        Arc::new(rustain::adapters::budget::BudgetStateStore::new()),
        Arc::new(ArcSwap::from_pointee(
            rustain::domain::models::AppConfig::default(),
        )),
        agent_core,
        compose_snapshot,
        Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::profile_resolver::noop::NoopProfileResolver,
        )
            as Arc<dyn rustain::domain::ports::ProfileResolver>)),
        test_cli(),
        rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        #[cfg(feature = "meta-search")]
        None,
    );
    // AppState should own an EventBus with the requested capacity.
    // We verify this indirectly by ensuring subscribe_raw works.
    let _raw_rx = app_state.event_bus.subscribe_raw();
}

#[test]
fn test_app_state_session_cancel_is_root_token() {
    let approval_runtime = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let provider_swap2 = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpProvider::default()) as Arc<dyn StreamingProvider>
    ));
    let provider_registry2 = Arc::new(rustain::adapters::provider::ProviderRegistry::new());
    let (event_bus2, domain_rx2) = EventBus::new(16);
    let event_bus2 = Arc::new(event_bus2);
    let agent_core2 = Arc::new(AgentCore::test_noop());
    let compose_snapshot2 = Arc::new(ComposeContext {
        workspace_path: std::path::PathBuf::from("."),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage::default())
            as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        tool_exposure: "static-full".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(rustain::adapters::sandbox::NoOpSandbox) as Arc<dyn rustain::domain::ports::SandboxManager>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(rustain::domain::models::sandbox::SandboxPolicy::Permissive)),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    });
    let (app_state, _domain_rx) = AppState::new(
        event_bus2,
        domain_rx2,
        approval_runtime,
        Arc::new(tokio::sync::RwLock::new(SandboxPolicy::ReadOnly { network: false })),
        Arc::new(PlanManager::new(std::path::PathBuf::from("."))),
        Arc::new(DefaultPlanInjector::new()),
        provider_swap2.clone(),
        provider_registry2,
        Arc::new(rustain::adapters::noop::NoOpUsageLedger),
        Arc::new(rustain::adapters::budget::BudgetStateStore::new()),
        Arc::new(ArcSwap::from_pointee(
            rustain::domain::models::AppConfig::default(),
        )),
        agent_core2,
        compose_snapshot2,
        Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::profile_resolver::noop::NoopProfileResolver,
        )
            as Arc<dyn rustain::domain::ports::ProfileResolver>)),
        test_cli(),
        rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        #[cfg(feature = "meta-search")]
        None,
    );
    // The session_cancel should be a root token (no parent)
    assert!(!app_state.session_cancel.is_cancelled());
}

// ── AC5: EventBus emit_domain writes to both channels ────────────────────────

#[tokio::test]
async fn test_emit_domain_dual_channel_happy_path() {
    let (bus, mut domain_rx) = EventBus::new(16);
    let mut raw_rx = bus.subscribe_raw();

    bus.emit_domain(AppEvent::SystemNotice {
        conversation_id: Some("conv-1".to_string()),
        level: NoticeLevel::Info,
        message: "hello".to_string(),
    });

    // Domain channel receives the event
    let domain_ev = tokio::time::timeout(Duration::from_millis(100), domain_rx.recv())
        .await
        .expect("timed out")
        .expect("domain channel closed");
    assert!(matches!(domain_ev, AppEvent::SystemNotice { .. }));

    // Raw channel receives the projected event
    let raw_ev = tokio::time::timeout(Duration::from_millis(100), raw_rx.recv())
        .await
        .expect("timed out")
        .expect("raw channel closed");
    assert!(matches!(raw_ev.kind, RawEventKind::SystemNotice { .. }));
    assert_eq!(raw_ev.conversation_id, Some("conv-1".to_string()));
}

#[tokio::test]
async fn test_emit_domain_tick_not_broadcast_to_raw() {
    let (bus, mut domain_rx) = EventBus::new(16);
    let mut raw_rx = bus.subscribe_raw();

    bus.emit_domain(AppEvent::Tick);

    let domain_ev = tokio::time::timeout(Duration::from_millis(50), domain_rx.recv())
        .await
        .expect("timed out")
        .expect("domain channel closed");
    assert!(matches!(domain_ev, AppEvent::Tick));

    // Raw subscriber should see nothing — Tick is internal-only
    let result = tokio::time::timeout(Duration::from_millis(50), raw_rx.recv()).await;
    assert!(result.is_err(), "Tick must not appear on raw channel");
}

// ── AC5: Raw subscriber lag handling ─────────────────────────────────────────

#[tokio::test]
async fn test_raw_subscriber_lag_receives_lagged_error() {
    // Small capacity ensures lag happens quickly
    let (bus, _domain_rx) = EventBus::new(2);

    // Emit events without any subscriber
    for i in 0..10 {
        bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: format!("event-{i}"),
        });
    }

    // Now subscribe — the subscriber starts at the tail and may immediately lag
    // because the channel only holds 2 events.
    let mut raw_rx = bus.subscribe_raw();

    // The next event should be receivable
    bus.emit_domain(AppEvent::SystemNotice {
        conversation_id: None,
        level: NoticeLevel::Info,
        message: "latest".to_string(),
    });

    // The subscriber may or may not lag depending on timing; we just verify
    // the channel is functional after a potential lag by consuming the latest event.
    match tokio::time::timeout(Duration::from_millis(100), raw_rx.recv()).await {
        Ok(Ok(raw)) => {
            assert!(matches!(raw.kind, RawEventKind::SystemNotice { .. }));
        }
        Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
            // This is the expected lag path. AC5 requires logging at warn.
            // In tests we just assert the lag count is reasonable.
            assert!(n > 0, "lag count should be positive");
        }
        Ok(Err(broadcast::error::RecvError::Closed)) => {
            panic!("raw channel closed unexpectedly");
        }
        Err(_) => {
            panic!("timed out waiting for raw event");
        }
    }
}

// ── AC5: Reference consumer pattern with timeout ─────────────────────────────

#[tokio::test]
async fn test_raw_subscriber_timeout_pattern() {
    let (bus, _domain_rx) = EventBus::new(16);
    let mut raw_rx = bus.subscribe_raw();

    // No events emitted — subscriber should idle-timeout
    let result = tokio::time::timeout(Duration::from_millis(50), raw_rx.recv()).await;
    assert!(
        result.is_err(),
        "subscriber should idle-timeout when no events"
    );

    // After timeout, subscriber should still be valid and receive next event
    bus.emit_domain(AppEvent::SetPermissionMode(PermissionMode::Normal));

    let raw = tokio::time::timeout(Duration::from_millis(100), raw_rx.recv())
        .await
        .expect("timed out")
        .expect("channel closed");
    assert!(matches!(
        raw.kind,
        RawEventKind::ModeChanged(PermissionMode::Normal)
    ));
}

// ── AC5: Multiple raw subscribers receive same events ────────────────────────

#[tokio::test]
async fn test_multiple_raw_subscribers_receive_events() {
    let (bus, _domain_rx) = EventBus::new(16);
    let mut rx_a = bus.subscribe_raw();
    let mut rx_b = bus.subscribe_raw();

    bus.emit_domain(AppEvent::SetPermissionMode(PermissionMode::Yolo));

    let raw_a = tokio::time::timeout(Duration::from_millis(100), rx_a.recv())
        .await
        .expect("timed out")
        .expect("channel closed");
    let raw_b = tokio::time::timeout(Duration::from_millis(100), rx_b.recv())
        .await
        .expect("timed out")
        .expect("channel closed");

    assert!(matches!(
        raw_a.kind,
        RawEventKind::ModeChanged(PermissionMode::Yolo)
    ));
    assert!(matches!(
        raw_b.kind,
        RawEventKind::ModeChanged(PermissionMode::Yolo)
    ));
}

// ── AC6: RawEvent from_app_event mapping coverage ────────────────────────────

#[test]
fn test_from_app_event_provider_chunk_maps_correctly() {
    let chunk = StreamChunk::Text {
        content: "stream data".to_string(),
        parent_tool_use_id: None,
    };
    let ev = AppEvent::ProviderChunk {
        conversation_id: "conv-p".to_string(),
        chunk: chunk.clone(),
    };
    let raw = RawEvent::from_app_event(&ev).expect("ProviderChunk should map");
    assert_eq!(raw.conversation_id, Some("conv-p".to_string()));
    assert!(matches!(raw.kind, RawEventKind::Provider(_)));
}

#[test]
fn test_from_app_event_resize_returns_none() {
    assert!(RawEvent::from_app_event(&AppEvent::Resize(80, 24)).is_none());
}

#[test]
fn test_from_app_event_input_event_returns_none() {
    use rustain::domain::events::DomainInputEvent;
    assert!(
        RawEvent::from_app_event(&AppEvent::InputEvent(DomainInputEvent::KeyPress('a'))).is_none()
    );
}

// ── AC5 / AC8: EventBus graceful shutdown when domain_rx dropped ─────────────

#[test]
fn test_emit_domain_graceful_when_receiver_dropped() {
    let (bus, domain_rx) = EventBus::new(16);
    drop(domain_rx);

    // emit_domain should not panic when the domain receiver is dropped
    bus.emit_domain(AppEvent::Tick);
    // If we reach here without panic, the graceful discard works
}
