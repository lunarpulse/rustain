//! Profile-switch request handler — Story 8.4 AC-4.
//!
//! `handle_profile_switch_requested` executes the Hot phase synchronously and
//! returns `RequestSpawn(ProfileSwap {...})` for the Warm+Cold continuation.
//! `handle_profile_swap_continuation` is invoked from the dispatch-site
//! `tokio::spawn` body and returns `Result<AppEvent, AppEvent>` for emission.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::adapters::tui::handlers::HandlerOutcome;
use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::errors::TransitionError;
use crate::domain::models::{PortDimension, ProfileIdentityColor, TransitionState};
use crate::domain::ports::{
    ChannelPort, MemoryPort, ProfileResolver, SchedulerPort, SessionPort,
};
use crate::domain::services::swap_tier::{PortDiff, SwapPolicy, SwapTier, TransitionPlan};
use crate::infrastructure::composition::ComposeContext;
use crate::infrastructure::runtime::agent_core::AgentCore;

static SWITCH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

pub struct SwitchGuard;

impl SwitchGuard {
    pub fn acquire() -> Option<Self> {
        if SWITCH_IN_PROGRESS
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Some(Self)
        } else {
            None
        }
    }
}

impl Drop for SwitchGuard {
    fn drop(&mut self) {
        SWITCH_IN_PROGRESS.store(false, Ordering::Release);
    }
}

fn port_name(port: PortDimension) -> &'static str {
    match port {
        PortDimension::Persona => "persona",
        PortDimension::Memory => "memory",
        PortDimension::Session => "session",
        PortDimension::Tools => "tools",
        PortDimension::Channels => "channels",
        PortDimension::Scheduler => "scheduler",
        PortDimension::Context => "context",
    }
}

#[allow(dead_code)]
pub async fn handle_profile_switch_requested(
    _state: &mut TuiState,
    agent_core: &Arc<AgentCore>,
    compose_snapshot: &Arc<ComposeContext>,
    app_config: &Arc<ArcSwap<crate::domain::models::AppConfig>>,
    profile_resolver: &Arc<ArcSwap<Arc<dyn ProfileResolver>>>,
    target_name: String,
) -> HandlerOutcome {
    let _guard = match SwitchGuard::acquire() {
        Some(g) => g,
        None => {
            return HandlerOutcome::Notify(AppEvent::ProfileSwitchFailed {
                profile_name: target_name,
                error: "Another profile switch is already in progress".to_string(),
                rolled_back: true,
            });
        }
    };

    tracing::info!(profile = %target_name, "Profile switch requested");

    let current_config = app_config.load_full();
    let current_name = current_config.active_profile.clone();

    if current_name == target_name {
        return HandlerOutcome::Notify(AppEvent::ProfileSwitched {
            profile_name: target_name,
            identity_color: ProfileIdentityColor(0),
            summary: "no changes — already active".to_string(),
            warm_cold_pending: false,
        });
    }

    let current_resolver = profile_resolver.load_full();
    let current_selection = match current_resolver.resolve_active() {
        Some(r) => r.selection,
        None => {
            return HandlerOutcome::Notify(AppEvent::ProfileSwitchFailed {
                profile_name: target_name,
                error: "Could not resolve current profile".to_string(),
                rolled_back: true,
            });
        }
    };

    let profiles_dir = match dirs::config_dir() {
        Some(d) => d.join("rustain").join("profiles"),
        None => {
            return HandlerOutcome::Notify(AppEvent::ProfileSwitchFailed {
                profile_name: target_name,
                error: "Could not determine config directory".to_string(),
                rolled_back: true,
            });
        }
    };

    let target_resolver =
        match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
            &target_name,
            profiles_dir.clone(),
        ) {
            Ok(r) => r,
            Err(e) => {
                return HandlerOutcome::Notify(AppEvent::ProfileSwitchFailed {
                    profile_name: target_name,
                    error: format!("Profile not found: {e}"),
                    rolled_back: true,
                });
            }
        };

    let target_resolved = match target_resolver.resolve_active() {
        Some(r) => r,
        None => {
            return HandlerOutcome::Notify(AppEvent::ProfileSwitchFailed {
                profile_name: target_name,
                error: "Could not resolve target profile".to_string(),
                rolled_back: true,
            });
        }
    };

    let identity_color = target_resolver
        .list_profiles()
        .into_iter()
        .find(|p| p.name == target_name)
        .map(|p| p.identity_color)
        .unwrap_or_else(|| {
            crate::domain::services::identity_color::derive_identity_color(&target_name, None)
        });

    let plan = TransitionPlan::from_selections(
        &current_selection.dimensions,
        &target_resolved.selection.dimensions,
        &target_name,
        identity_color.0,
    );

    let hot_count = plan.diffs.iter().filter(|d| d.tier == SwapTier::Hot).count();
    let warm_count = plan.diffs.iter().filter(|d| d.tier == SwapTier::Warm).count();
    let cold_count = plan.diffs.iter().filter(|d| d.tier == SwapTier::Cold).count();
    tracing::info!(
        profile = %target_name,
        hot_count, warm_count, cold_count,
        "Profile transition plan computed"
    );

    if plan.diffs.is_empty() {
        return HandlerOutcome::Notify(AppEvent::ProfileSwitched {
            profile_name: target_name,
            identity_color,
            summary: "no changes".to_string(),
            warm_cold_pending: false,
        });
    }

    let hot_diffs: Vec<_> = plan
        .diffs
        .iter()
        .filter(|d| d.tier == SwapTier::Hot)
        .cloned()
        .collect();

    let warm_cold_diffs: Vec<_> = plan
        .diffs
        .iter()
        .filter(|d| d.tier != SwapTier::Hot)
        .cloned()
        .collect();

    let summary = format!(
        "{} hot, {} warm, {} cold",
        hot_diffs.len(),
        warm_cold_diffs.iter().filter(|d| d.tier == SwapTier::Warm).count(),
        warm_cold_diffs.iter().filter(|d| d.tier == SwapTier::Cold).count(),
    );

    let _prev_persona = agent_core.persona.load_full();
    let _prev_tools = agent_core.tools.load_full();
    let _prev_context = agent_core.context.load_full();

    // Pre-build all hot adapters before swapping any (atomicity per P-2)
    let mut new_persona: Option<Arc<dyn crate::domain::ports::PersonaPort>> = None;
    let mut new_tools: Option<Arc<dyn crate::domain::ports::ToolSetPort>> = None;
    let mut new_context: Option<Arc<dyn crate::domain::ports::ContextPort>> = None;

    for diff in &hot_diffs {
        match diff.port {
            PortDimension::Persona => {
                match crate::infrastructure::composition::build_persona(
                    &diff.to_adapter, None, compose_snapshot,
                ) {
                    Ok(a) => new_persona = Some(a),
                    Err(e) => {
                        return HandlerOutcome::Notify(AppEvent::ProfileSwitchFailed {
                            profile_name: target_name,
                            error: format!("Failed to build persona '{}': {e}", diff.to_adapter),
                            rolled_back: true,
                        });
                    }
                }
            }
            PortDimension::Tools => {
                match crate::infrastructure::composition::build_tools(
                    &diff.to_adapter, None, compose_snapshot,
                ) {
                    Ok(a) => new_tools = Some(a),
                    Err(e) => {
                        return HandlerOutcome::Notify(AppEvent::ProfileSwitchFailed {
                            profile_name: target_name,
                            error: format!("Failed to build tools '{}': {e}", diff.to_adapter),
                            rolled_back: true,
                        });
                    }
                }
            }
            PortDimension::Context => {
                match crate::infrastructure::composition::build_context(
                    &diff.to_adapter, None, compose_snapshot,
                ) {
                    Ok(a) => new_context = Some(a),
                    Err(e) => {
                        return HandlerOutcome::Notify(AppEvent::ProfileSwitchFailed {
                            profile_name: target_name,
                            error: format!("Failed to build context '{}': {e}", diff.to_adapter),
                            rolled_back: true,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // Atomic swap all hot adapters (ArcSwap is lock-free)
    if let Some(a) = new_persona {
        agent_core.persona.store(Arc::new(a));
        tracing::debug!(port = ?PortDimension::Persona, "Hot swap executed");
    }
    if let Some(a) = new_tools {
        agent_core.tools.store(Arc::new(a));
        tracing::debug!(port = ?PortDimension::Tools, "Hot swap executed");
    }
    if let Some(a) = new_context {
        agent_core.context.store(Arc::new(a));
        tracing::debug!(port = ?PortDimension::Context, "Hot swap executed");
    }

    if warm_cold_diffs.is_empty() {
        let mut new_config = (*current_config).clone();
        new_config.active_profile = target_name.clone();
        app_config.store(Arc::new(new_config));

        let new_resolver: Arc<dyn ProfileResolver> = Arc::new(target_resolver);
        profile_resolver.store(Arc::new(new_resolver));

        HandlerOutcome::Notify(AppEvent::ProfileSwitched {
            profile_name: target_name,
            identity_color,
            summary,
            warm_cold_pending: false,
        })
    } else {
        HandlerOutcome::RequestSpawn(crate::adapters::tui::handlers::SpawnRequest::ProfileSwap {
            profile_name: target_name,
            identity_color,
            warm_cold_diffs,
            agent_core: Arc::clone(agent_core),
            compose_snapshot: Arc::clone(compose_snapshot),
            profile_resolver: Arc::clone(profile_resolver),
            app_config: Arc::clone(app_config),
            guard: Some(_guard),
            new_resolver: Arc::new(target_resolver),
        })
    }
}

pub async fn handle_profile_swap_continuation(
    diffs: Vec<PortDiff>,
    agent_core: Arc<AgentCore>,
    compose_snapshot: Arc<ComposeContext>,
    profile_name: String,
    identity_color: ProfileIdentityColor,
    app_config: Arc<ArcSwap<crate::domain::models::AppConfig>>,
    profile_resolver: Arc<ArcSwap<Arc<dyn ProfileResolver>>>,
    new_resolver: Arc<dyn ProfileResolver>,
) -> Result<AppEvent, AppEvent> {
    let warm_diffs: Vec<_> = diffs.iter().filter(|d| d.tier == SwapTier::Warm).cloned().collect();
    let cold_diffs: Vec<_> = diffs.iter().filter(|d| d.tier == SwapTier::Cold).cloned().collect();

    let warm_count = warm_diffs.len();
    let cold_count = cold_diffs.len();

    // Snapshot current adapters for rollback (ArcSwap load_full → Arc<Arc<dyn Port>>)
    let prev_memory = agent_core.memory.load_full();
    let prev_session = agent_core.session.load_full();
    let prev_channels = agent_core.channels.load_full();
    let prev_scheduler = agent_core.scheduler.load_full();

    let start = std::time::Instant::now();

    // Warm phase
    for diff in &warm_diffs {
        if let Err(e) = execute_warm_swap(diff, &agent_core, &compose_snapshot).await {
            tracing::warn!(
                port = ?diff.port,
                error = %e,
                "Warm swap rolled back — restoring previous adapter"
            );
            agent_core.memory.store(prev_memory);
            agent_core.session.store(prev_session);
            return Err(AppEvent::ProfileSwitchFailed {
                profile_name,
                error: e.to_string(),
                rolled_back: true,
            });
        }
        tracing::info!(
            port = ?diff.port,
            from = %diff.from_adapter,
            to = %diff.to_adapter,
            policy = ?diff.policy,
            elapsed_ms = start.elapsed().as_millis(),
            "Warm swap complete"
        );
    }

    // Cold phase
    for diff in &cold_diffs {
        match execute_cold_swap(diff, &agent_core, &compose_snapshot).await {
            Ok(_) => {
                tracing::info!(
                    port = ?diff.port,
                    from = %diff.from_adapter,
                    to = %diff.to_adapter,
                    elapsed_ms = start.elapsed().as_millis(),
                    "Cold swap complete (loop restart)"
                );
            }
            Err(e) => {
                tracing::error!(
                    port = ?diff.port,
                    error = %e,
                    "Cold swap failed — start_loop returned error"
                );
                {
                    let current_channels = agent_core.channels.load_full();
                    let _ = current_channels.shutdown_loop().await;
                }
                {
                    let current_scheduler = agent_core.scheduler.load_full();
                    let _ = current_scheduler.shutdown_loop().await;
                }
                agent_core.channels.store(prev_channels);
                agent_core.scheduler.store(prev_scheduler);
                return Err(AppEvent::ProfileSwitchFailed {
                    profile_name,
                    error: e.to_string(),
                    rolled_back: true,
                });
            }
        }
    }

    let total_elapsed_ms = start.elapsed().as_millis();

    let current_config = app_config.load_full();
    let mut new_config = (*current_config).clone();
    new_config.active_profile = profile_name.clone();
    app_config.store(Arc::new(new_config));
    profile_resolver.store(Arc::new(new_resolver));

    tracing::info!(profile = %profile_name, total_elapsed_ms, "Profile switch complete");

    Ok(AppEvent::ProfileSwitched {
        profile_name,
        identity_color,
        summary: format!("{warm_count} warm, {cold_count} cold"),
        warm_cold_pending: false,
    })
}

async fn execute_warm_swap(
    diff: &PortDiff,
    agent_core: &AgentCore,
    compose_snapshot: &ComposeContext,
) -> Result<(), TransitionError> {
    match diff.port {
        PortDimension::Memory => {
            let old_arc = agent_core.memory.load_full();
            let state = old_arc.prepare_detach().await?;

            let new_adapter =
                crate::infrastructure::composition::build_memory(&diff.to_adapter, None, compose_snapshot)
                    .map_err(|e| TransitionError::PrepareFailed {
                        port_type: "memory",
                        adapter_id: diff.to_adapter.clone(),
                        reason: e.to_string(),
                    })?;

            // TODO Story 8.4-FU1: Merge and Selective policies currently fall through to CarryOver
            // until real (non-NoOp) memory/session adapters land in Epic 12+.
            let effective_state = match diff.policy {
                SwapPolicy::CarryOver | SwapPolicy::Merge | SwapPolicy::Selective => state,
                SwapPolicy::FreshStart => TransitionState::empty("memory"),
            };
            new_adapter.receive_state(effective_state).await?;
            agent_core.memory.store(Arc::new(new_adapter));
            let current = agent_core.memory.load_full();
            current.post_transition_verify().await?;
        }
        PortDimension::Session => {
            let old_arc = agent_core.session.load_full();
            let state = old_arc.prepare_detach().await?;

            let new_adapter =
                crate::infrastructure::composition::build_session(&diff.to_adapter, None, compose_snapshot)
                    .map_err(|e| TransitionError::PrepareFailed {
                        port_type: "session",
                        adapter_id: diff.to_adapter.clone(),
                        reason: e.to_string(),
                    })?;

            // TODO Story 8.4-FU1: Merge and Selective policies currently fall through to CarryOver
            // until real (non-NoOp) memory/session adapters land in Epic 12+.
            let effective_state = match diff.policy {
                SwapPolicy::CarryOver | SwapPolicy::Merge | SwapPolicy::Selective => state,
                SwapPolicy::FreshStart => TransitionState::empty("session"),
            };
            new_adapter.receive_state(effective_state).await?;
            agent_core.session.store(Arc::new(new_adapter));
            let current = agent_core.session.load_full();
            current.post_transition_verify().await?;
        }
        _ => {
            return Err(TransitionError::PrepareFailed {
                port_type: port_name(diff.port),
                adapter_id: diff.to_adapter.clone(),
                reason: format!("{:?} is not a Warm-tier port", diff.port),
            });
        }
    }
    Ok(())
}

async fn execute_cold_swap(
    diff: &PortDiff,
    agent_core: &AgentCore,
    compose_snapshot: &ComposeContext,
) -> Result<(), TransitionError> {
    match diff.port {
        PortDimension::Channels => {
            let old_arc = agent_core.channels.load_full();
            old_arc.shutdown_loop().await?;

            let new_adapter =
                crate::infrastructure::composition::build_channels(&diff.to_adapter, None, compose_snapshot)
                    .map_err(|e| TransitionError::RestartFailed {
                        port_type: "channels",
                        adapter_id: diff.to_adapter.clone(),
                        reason: e.to_string(),
                    })?;

            agent_core.channels.store(Arc::new(new_adapter));
            let current = agent_core.channels.load_full();
            current.start_loop().await?;
        }
        PortDimension::Scheduler => {
            let old_arc = agent_core.scheduler.load_full();
            old_arc.shutdown_loop().await?;

            let new_adapter =
                crate::infrastructure::composition::build_scheduler(&diff.to_adapter, None, compose_snapshot)
                    .map_err(|e| TransitionError::RestartFailed {
                        port_type: "scheduler",
                        adapter_id: diff.to_adapter.clone(),
                        reason: e.to_string(),
                    })?;

            agent_core.scheduler.store(Arc::new(new_adapter));
            let current = agent_core.scheduler.load_full();
            current.start_loop().await?;
        }
        _ => {
            return Err(TransitionError::RestartFailed {
                port_type: port_name(diff.port),
                adapter_id: diff.to_adapter.clone(),
                reason: format!("{:?} is not a Cold-tier port", diff.port),
            });
        }
    }
    Ok(())
}
