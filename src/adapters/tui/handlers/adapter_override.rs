//! Adapter-override handler — Story 8.5 AC-6, AC-7.
//!
//! `handle_apply_adapter_override` executes a Hot-tier sync override:
//! compose → agent_core.store() → record in session_overrides.
//! Warm protocol NOT invoked (state-loss accepted per Decision Gate 1.8).
//!
//! `handle_clear_adapter_override` restores the profile-default adapter
//! for a single port and removes the override entry from session_overrides.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::adapters::tui::handlers::HandlerOutcome;
use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::models::profile::{AdapterRef, PortDimension};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::adapter_overlay;
use crate::infrastructure::composition::{self, ComposeContext};
use crate::infrastructure::runtime::agent_core::AgentCore;

pub async fn handle_apply_adapter_override(
    state: &mut TuiState,
    agent_core: &Arc<AgentCore>,
    compose_snapshot: &Arc<ComposeContext>,
    port: PortDimension,
    adapter_ref: AdapterRef,
) -> HandlerOutcome {
    let previous_adapter_name = state
        .session_overrides
        .get(&port)
        .map(|r| r.adapter.clone())
        .unwrap_or_else(|| {
            adapter_overlay::port_label(port).to_string()
        });

    match composition::build_for_port(port, &adapter_ref, compose_snapshot) {
        Ok(built) => {
            let adapter_name = adapter_ref.adapter.clone();
            agent_core.store_for_port(built);
            state
                .session_overrides
                .insert(port, adapter_ref.clone());

            tracing::info!(
                port = ?port,
                adapter = %adapter_name,
                source = "slash|cli|palette",
                "Session adapter override applied"
            );

            HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden {
                port,
                adapter_name,
                previous_adapter_name,
            })
        }
        Err(e) => {
            let error = e.to_string();
            tracing::warn!(
                port = ?port,
                requested = %adapter_ref.adapter,
                error = %error,
                "Session adapter override failed"
            );

            HandlerOutcome::Notify(AppEvent::SessionAdapterOverrideFailed {
                port,
                requested_adapter: adapter_ref.adapter.clone(),
                error,
            })
        }
    }
}

pub async fn handle_clear_adapter_override(
    state: &mut TuiState,
    agent_core: &Arc<AgentCore>,
    compose_snapshot: &Arc<ComposeContext>,
    profile_resolver: &Arc<ArcSwap<Arc<dyn ProfileResolver>>>,
    port: PortDimension,
) -> HandlerOutcome {
    // Determine the profile-default adapter name for this port.
    // We need to resolve the active profile to find the default.
    let resolver = profile_resolver.load_full();
    let default_name: Option<String> = if let Some(resolved) = resolver.resolve_active() {
        resolved
            .selection
            .dimensions
            .get(&port)
            .map(|r| r.adapter.clone())
    } else {
        None
    };

    let adapter_name = default_name.unwrap_or_else(|| match port {
        PortDimension::Persona => "coding".to_string(),
        PortDimension::Memory => "noop".to_string(),
        PortDimension::Session => "basic".to_string(),
        PortDimension::Tools => "builtin-full".to_string(),
        PortDimension::Channels => "terminal".to_string(),
        PortDimension::Scheduler => "none".to_string(),
        PortDimension::Context => "default".to_string(),
    });

    let adapter_ref = AdapterRef {
        adapter: adapter_name.clone(),
        _config: None,
    };

    match composition::build_for_port(port, &adapter_ref, compose_snapshot) {
        Ok(built) => {
            let restored_name = adapter_name.clone();
            agent_core.store_for_port(built);
            state.session_overrides.remove(&port);

            tracing::info!(
                port = ?port,
                restored_to = %restored_name,
                "Session adapter override cleared"
            );

            HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden {
                port,
                adapter_name: restored_name,
                previous_adapter_name: "(override)".to_string(),
            })
        }
        Err(e) => {
            let error = e.to_string();
            tracing::warn!(
                port = ?port,
                requested = %adapter_name,
                error = %error,
                "Session adapter override clear failed"
            );

            HandlerOutcome::Notify(AppEvent::SessionAdapterOverrideFailed {
                port,
                requested_adapter: adapter_name,
                error,
            })
        }
    }
}
