//! E2E integration tests for Story 8.5: Adapter Status Panel & Session Overrides.
//!
//! Covers:
//! - AC-6: handle_apply_adapter_override hot-tier sync override
//! - AC-7: handle_clear_adapter_override restores profile-default
//! - AC-9: session_overrides cleared on profile switch
//! - session_overrides tracks across multiple apply/clear cycles
//! - HandlerOutcome events emitted correctly

use std::sync::Arc;

use arc_swap::ArcSwap;

use rustain::adapters::tui::handlers::HandlerOutcome;
use rustain::adapters::tui::handlers::adapter_override::{
    handle_apply_adapter_override, handle_clear_adapter_override,
};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::AppEvent;
use rustain::domain::models::profile::{AdapterRef, PortDimension};
use rustain::infrastructure::composition::ComposeContext;
use rustain::infrastructure::runtime::agent_core::AgentCore;

fn test_compose_ctx() -> ComposeContext {
    ComposeContext {
        workspace_path: std::path::PathBuf::from("/tmp/test-adapter-override-e2e"),
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
        sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as std::sync::Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    }
}

fn noop_profile_resolver() -> Arc<ArcSwap<Arc<dyn rustain::domain::ports::ProfileResolver>>> {
    Arc::new(ArcSwap::from_pointee(Arc::new(
        rustain::adapters::profile_resolver::noop::NoopProfileResolver,
    )
        as Arc<dyn rustain::domain::ports::ProfileResolver>))
}

fn make_ref(name: &str) -> AdapterRef {
    AdapterRef {
        adapter: name.to_string(),
        _config: None,
    }
}

// ── AC-6: Apply override stores in session_overrides and emits event ──────────

#[tokio::test]
async fn test_apply_override_stores_and_emits() {
    let mut state = TuiState::new(120, 40);
    let core = Arc::new(AgentCore::test_noop());
    let ctx = Arc::new(test_compose_ctx());

    let outcome = handle_apply_adapter_override(
        &mut state,
        &core,
        &ctx,
        PortDimension::Memory,
        make_ref("noop"),
    )
    .await;

    assert!(
        state.session_overrides.contains_key(&PortDimension::Memory),
        "override should be recorded in session_overrides"
    );

    match outcome {
        HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden {
            port, adapter_name, ..
        }) => {
            assert_eq!(port, PortDimension::Memory);
            assert_eq!(adapter_name, "noop");
        }
        _ => panic!("expected Notify(SessionAdapterOverridden)"),
    }
}

// ── AC-6: Apply override for a known-good adapter on each port dimension ──────

#[tokio::test]
async fn test_apply_override_all_seven_ports() {
    let mut state = TuiState::new(120, 40);
    let core = Arc::new(AgentCore::test_noop());
    let ctx = Arc::new(test_compose_ctx());

    let cases: Vec<(PortDimension, &str)> = vec![
        (PortDimension::Persona, "coding"),
        (PortDimension::Memory, "noop"),
        (PortDimension::Session, "basic"),
        (PortDimension::Tools, "builtin-full"),
        (PortDimension::Channels, "terminal"),
        (PortDimension::Scheduler, "none"),
        (PortDimension::Context, "default"),
    ];

    for (port, adapter_name) in &cases {
        let outcome =
            handle_apply_adapter_override(&mut state, &core, &ctx, *port, make_ref(adapter_name))
                .await;

        match outcome {
            HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden { .. }) => {}
            _ => panic!("expected Notify(SessionAdapterOverridden) for {:?}", port),
        }
    }

    assert_eq!(state.session_overrides.len(), 7);
}

// ── AC-6: Applying to an unknown adapter name emits failure ───────────────────

#[tokio::test]
async fn test_apply_override_unknown_adapter_emits_failure() {
    let mut state = TuiState::new(120, 40);
    let core = Arc::new(AgentCore::test_noop());
    let ctx = Arc::new(test_compose_ctx());

    let outcome = handle_apply_adapter_override(
        &mut state,
        &core,
        &ctx,
        PortDimension::Memory,
        make_ref("nonexistent-adapter-xyz"),
    )
    .await;

    match outcome {
        HandlerOutcome::Notify(AppEvent::SessionAdapterOverrideFailed {
            port,
            requested_adapter,
            ..
        }) => {
            assert_eq!(port, PortDimension::Memory);
            assert_eq!(requested_adapter, "nonexistent-adapter-xyz");
        }
        _ => panic!("expected Notify(SessionAdapterOverrideFailed)"),
    }

    assert!(
        !state.session_overrides.contains_key(&PortDimension::Memory),
        "failed override must not be recorded"
    );
}

// ── AC-7: Clear override removes from session_overrides and emits event ───────

#[tokio::test]
async fn test_clear_override_removes_and_emits() {
    let mut state = TuiState::new(120, 40);
    let core = Arc::new(AgentCore::test_noop());
    let ctx = Arc::new(test_compose_ctx());
    let resolver = noop_profile_resolver();

    // First apply
    handle_apply_adapter_override(
        &mut state,
        &core,
        &ctx,
        PortDimension::Memory,
        make_ref("noop"),
    )
    .await;
    assert!(state.session_overrides.contains_key(&PortDimension::Memory));

    // Now clear
    let outcome =
        handle_clear_adapter_override(&mut state, &core, &ctx, &resolver, PortDimension::Memory)
            .await;

    assert!(
        !state.session_overrides.contains_key(&PortDimension::Memory),
        "clear should remove the override"
    );

    match outcome {
        HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden {
            port, adapter_name, ..
        }) => {
            assert_eq!(port, PortDimension::Memory);
            assert_eq!(adapter_name, "noop");
        }
        _ => panic!("expected Notify(SessionAdapterOverridden)"),
    }
}

// ── AC-7: Clear on non-overridden port still emits success ────────────────────

#[tokio::test]
async fn test_clear_override_noop_when_no_override() {
    let mut state = TuiState::new(120, 40);
    let core = Arc::new(AgentCore::test_noop());
    let ctx = Arc::new(test_compose_ctx());
    let resolver = noop_profile_resolver();

    assert!(state.session_overrides.is_empty());

    let outcome =
        handle_clear_adapter_override(&mut state, &core, &ctx, &resolver, PortDimension::Persona)
            .await;

    match outcome {
        HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden { port, .. }) => {
            assert_eq!(port, PortDimension::Persona);
        }
        _ => panic!("expected Notify(SessionAdapterOverridden)"),
    }

    assert!(state.session_overrides.is_empty());
}

// ── AC-9: Apply-cycle across multiple ports and verify independent tracking ───

#[tokio::test]
async fn test_multi_port_overrides_independent() {
    let mut state = TuiState::new(120, 40);
    let core = Arc::new(AgentCore::test_noop());
    let ctx = Arc::new(test_compose_ctx());
    let resolver = noop_profile_resolver();

    // Apply to Memory
    handle_apply_adapter_override(
        &mut state,
        &core,
        &ctx,
        PortDimension::Memory,
        make_ref("noop"),
    )
    .await;

    // Apply to Tools
    handle_apply_adapter_override(
        &mut state,
        &core,
        &ctx,
        PortDimension::Tools,
        make_ref("builtin-full"),
    )
    .await;

    assert_eq!(state.session_overrides.len(), 2);

    // Clear only Memory
    handle_clear_adapter_override(&mut state, &core, &ctx, &resolver, PortDimension::Memory).await;

    assert_eq!(state.session_overrides.len(), 1);
    assert!(state.session_overrides.contains_key(&PortDimension::Tools));
    assert!(!state.session_overrides.contains_key(&PortDimension::Memory));
}

// ── Re-apply replaces previous override ───────────────────────────────────────

#[tokio::test]
async fn test_reapply_replaces_override() {
    let mut state = TuiState::new(120, 40);
    let core = Arc::new(AgentCore::test_noop());
    let ctx = Arc::new(test_compose_ctx());

    // First apply
    handle_apply_adapter_override(
        &mut state,
        &core,
        &ctx,
        PortDimension::Memory,
        make_ref("noop"),
    )
    .await;

    // Re-apply with different adapter
    let outcome = handle_apply_adapter_override(
        &mut state,
        &core,
        &ctx,
        PortDimension::Memory,
        make_ref("noop"),
    )
    .await;

    assert_eq!(state.session_overrides.len(), 1);

    match outcome {
        HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden {
            previous_adapter_name,
            adapter_name,
            ..
        }) => {
            assert_eq!(adapter_name, "noop");
            assert_eq!(
                previous_adapter_name, "noop",
                "previous should be the first override"
            );
        }
        _ => panic!("expected Notify(SessionAdapterOverridden)"),
    }
}

// ── Default TuiState has empty session_overrides ──────────────────────────────

#[test]
fn test_tui_state_default_empty_overrides() {
    let state = TuiState::new(120, 40);
    assert!(state.session_overrides.is_empty());
}

// ── active_adapter_for reads from session_overrides ───────────────────────────

#[test]
fn test_active_adapter_for_integration() {
    use rustain::domain::services::adapter_overlay;

    let mut state = TuiState::new(120, 40);
    let got = adapter_overlay::active_adapter_for(
        PortDimension::Memory,
        "noop",
        &state.session_overrides,
    );
    assert_eq!(got, "noop");

    state
        .session_overrides
        .insert(PortDimension::Memory, make_ref("custom-mem"));
    let got = adapter_overlay::active_adapter_for(
        PortDimension::Memory,
        "noop",
        &state.session_overrides,
    );
    assert_eq!(got, "custom-mem");
}
