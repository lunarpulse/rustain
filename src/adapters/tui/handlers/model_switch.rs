//! Model-switch handlers — Story 7.2 + 7.3. **Phase 2 Task 4 prototype
//! (load-bearing for ADR-08-01 §D2 spawn-stays invariant + multi-path
//! outcome flow per DGI-D).**
//!
//! Handles AppEvent + InputAction families (consumed via `event_loop.rs`
//! dispatch arms):
//! - `InputAction::SwitchModelProvider` → `handle_apply_model_switch`
//! - `InputAction::CompactThenSwitchModel` (post-compaction phase) → `handle_apply_model_switch`
//! - Health-check completion (post-`tokio::spawn` from RequestSpawn::HealthCheck) → `handle_complete_model_switch`
//! - Startup readiness (one-shot) → `handle_apply_startup_provider_fallback`
//!
//! Per ADR-08-01 §D2: the `tokio::spawn(...)` for the provider health-check
//! lives at the dispatch site (event_loop.rs) — NOT in this module. Handler
//! returns `HandlerOutcome::RequestSpawn(SpawnRequest::HealthCheck { ... })`
//! and the dispatch arm builds the future + stores the JoinHandle.
//!
//! Per DGI-D / Winston Task 1 sign-off (2026-05-16): `handle_apply_model_switch`
//! retains the inline `domain_tx.send` for 2 guard notices ("Unknown model",
//! "Cannot switch while streaming"). These are existing untagged bypasses
//! grandfathered into the `MAX_KNOWN_BYPASSES` count — preserving them is
//! net-zero on the ratchet.
//!
//! Per ADR-08-01 §D8.1 + AC-5 domain isolation: handlers take
//! `&dyn ProviderInfoPort` (domain port from `domain/ports/provider_info.rs`)
//! instead of `&AppState` + `&ProviderRouter`. Dispatch arm constructs
//! `AppContext::new(&app_state, &router)` and passes it as the port.

#![allow(dead_code)]

use tokio::sync::mpsc;

use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::presentation::{self, Surface, reducer};
use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::models::{Conversation, NoticeLevel, StreamingState};
use crate::domain::ports::ProviderInfoPort;

use super::{HandlerOutcome, SpawnRequest};

/// Locked columns a sign-in can unlock: column id → store key `/login` takes.
fn sign_in_map(
    app: &crate::domain::models::AppConfig,
) -> std::collections::HashMap<String, String> {
    crate::adapters::auth::host::login_targets(app)
        .into_iter()
        .flat_map(|target| {
            let id = target.id;
            target
                .entries
                .into_iter()
                .chain(std::iter::once(id.clone()))
                .map(move |entry| (entry, id.clone()))
        })
        .collect()
}

/// Opens the model picker (Ctrl+X M, `/models`, and after a sign-in) at
/// `column` when given, else at the active provider and model.
pub fn open_selector(
    state: &mut TuiState,
    info: &dyn ProviderInfoPort,
    app: &crate::domain::models::AppConfig,
    column: Option<&str>,
) {
    let returning = state.login.frame_intent
        == Some(crate::adapters::tui::state::LoginFrameIntent::ReturnToModel);
    if returning {
        state.login.frame_intent = None;
        reducer::handoff(state, Surface::Login, Surface::ModelSelector);
    } else {
        reducer::open(state, Surface::ModelSelector);
    }
    initialize_selector(state, info, app, column);
    if returning {
        crate::adapters::tui::app::advance_deferred_permission(state);
    }
}

fn initialize_selector(
    state: &mut TuiState,
    info: &dyn ProviderInfoPort,
    app: &crate::domain::models::AppConfig,
    column: Option<&str>,
) {
    if !reducer::initialize_surface(state, Surface::ModelSelector) {
        return;
    }
    let columns = crate::adapters::tui::state::model_selector_columns(info);
    if columns.is_empty() {
        reducer::close(state, Surface::ModelSelector);
        notifications::info(state, NoticeSource::Model, "No models available");
    } else {
        let active = column.map_or_else(
            || std::borrow::Cow::Owned(info.active_delegate_id()),
            std::borrow::Cow::Borrowed,
        );
        let model = state.selected_model.as_deref().unwrap_or(&app.model);
        state.model_selector.initialize(columns, &active, model);
        state.model_selector.sign_in = sign_in_map(app);
    }
    state.needs_redraw = true;
}

/// A provider's catalogue changed: an open picker rebuilds its columns (and
/// which locked columns can be signed into) in place. The in-flight marker is
/// owned by the refresh itself (it ends before this event is sent), so an
/// earlier event never clears a refresh that is still running.
pub fn on_catalog_refreshed(
    state: &mut TuiState,
    info: &dyn ProviderInfoPort,
    app: &crate::domain::models::AppConfig,
) {
    if presentation::is_open(state, Surface::ModelSelector) {
        state
            .model_selector
            .refresh_columns(crate::adapters::tui::state::model_selector_columns(info));
        state.model_selector.sign_in = sign_in_map(app);
    }
    state.needs_redraw = true;
}

/// Guards + provider-health resolution for model switching. Multi-path outcome:
///
/// - `HandlerOutcome::Quiet` when:
///   - Unknown model (inline `domain_tx.send` notice; grandfathered bypass)
///   - Streaming active (inline `domain_tx.send` notice; grandfathered bypass)
///   - Provider unhealthy + not found in router (state.feedback_blocks mutation)
///   - Context-window warning condition met (state.model_selector mutation; returns to await user choice)
///   - Healthy delegate (state.selected_model + state.feedback_blocks mutation via
///     `handle_complete_model_switch` intra-module call)
/// - `HandlerOutcome::RequestSpawn(SpawnRequest::HealthCheck { ... })` when:
///   - Unhealthy provider found via `info.get_provider(pid)` — caller spawns
///     `provider.health_check().await` and stores JoinHandle in
///     `pending_health_check: Option<(String, String, JoinHandle<...>)>`
///
/// Dispatch-arm pattern:
/// ```ignore
/// let app_context = AppContext::new(&app_state, &router);
/// match handlers::model_switch::handle_apply_model_switch(
///     &mut state, &app_context, &streaming, &domain_tx, &conversation,
///     provider_id, model_id,
/// ).await {
///     HandlerOutcome::Quiet => {}
///     HandlerOutcome::RequestSpawn(SpawnRequest::HealthCheck { provider_id, model_id, provider }) => {
///         let handle = tokio::spawn(async move { provider.health_check().await });
///         pending_health_check = Some((provider_id, model_id, handle));
///     }
///     _ => unreachable!("apply_model_switch only returns Quiet or RequestSpawn(HealthCheck)"),
/// }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn handle_apply_model_switch(
    state: &mut TuiState,
    info: &dyn ProviderInfoPort,
    streaming: &StreamingState,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
    conversation: &Conversation,
    provider_id: Option<String>,
    model_id: String,
) -> HandlerOutcome {
    let from_palette = state
        .modal_stack
        .top()
        .is_some_and(|f| f.surface == Surface::CommandPalette);
    let resolved_pid = match provider_id {
        Some(pid) => pid,
        None => {
            let active = info.active_delegate_id();
            match info.get_model_provider(&model_id, Some(&active)) {
                Some(pid) => pid,
                None => {
                    let _ = domain_tx.send(AppEvent::SystemNotice {
                        conversation_id: None,
                        level: NoticeLevel::Warning,
                        message: format!("Unknown model: {}", model_id),
                    });
                    if from_palette {
                        reducer::close(state, Surface::CommandPalette);
                    }
                    return HandlerOutcome::Quiet;
                }
            }
        }
    };

    if streaming.is_streaming {
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "Cannot switch model while streaming".to_string(),
        });
        if from_palette {
            reducer::close(state, Surface::CommandPalette);
        }
        return HandlerOutcome::Quiet;
    }

    let provider_desc = info
        .list_providers()
        .into_iter()
        .find(|p| p.provider_id == resolved_pid);
    let is_healthy = provider_desc.as_ref().is_some_and(|p| p.healthy);
    let provider_display_name = provider_desc
        .as_ref()
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| resolved_pid.clone());

    if !is_healthy {
        if from_palette {
            reducer::close(state, Surface::CommandPalette);
        }
        if let Some(provider) = info.get_provider(&resolved_pid) {
            state.model_selector.connecting = Some(provider_display_name.clone());
            state.needs_redraw = true;
            return HandlerOutcome::RequestSpawn(SpawnRequest::HealthCheck {
                provider_id: resolved_pid,
                model_id,
                provider,
            });
        } else {
            notifications::warning(
                state,
                NoticeSource::Model,
                format!("Provider '{}' not found", resolved_pid),
            );
            state.needs_redraw = true;
            return HandlerOutcome::Quiet;
        }
    }

    // A palette selection replaces its frame only once the model is known.
    let model_desc = info.get_model(&resolved_pid, &model_id);
    let context_window = model_desc.as_ref().map_or(0, |m| m.context_window);
    let model_display_name = model_desc
        .as_ref()
        .map(|m| m.display_name.clone())
        .unwrap_or_else(|| model_id.clone());

    if model_desc.is_some()
        && (from_palette || !presentation::is_open(state, Surface::ModelSelector))
    {
        let columns = crate::adapters::tui::state::model_selector_columns(info);
        if !columns.is_empty() {
            if from_palette {
                reducer::handoff(state, Surface::CommandPalette, Surface::ModelSelector);
            } else {
                reducer::open(state, Surface::ModelSelector);
            }
            if reducer::initialize_surface(state, Surface::ModelSelector) {
                state
                    .model_selector
                    .initialize(columns, &resolved_pid, &model_id);
            }
        } else if from_palette {
            reducer::close(state, Surface::CommandPalette);
        }
    } else if from_palette {
        reducer::close(state, Surface::CommandPalette);
    }

    if let Some(ref warning) = state.model_selector.pending_context_warning {
        if warning.model_id == model_id && warning.provider_id == resolved_pid {
            state.model_selector.pending_context_warning = None;
        }
    } else {
        let current_tokens = conversation.usage.as_ref().map_or(0, |u| u.input_tokens);
        if context_window > 0 && current_tokens > context_window {
            state.model_selector.pending_context_warning =
                Some(crate::adapters::tui::state::ContextWarning {
                    provider_id: resolved_pid.clone(),
                    model_id: model_id.clone(),
                    model_display_name: model_display_name.clone(),
                    context_window,
                    current_tokens,
                });
            state.needs_redraw = true;
            return HandlerOutcome::Quiet;
        }
    }

    handle_complete_model_switch(state, info, &resolved_pid, &model_id).await
}

/// Final state mutation after model + provider are validated. Returns `Quiet`
/// (state mutation only: `state.selected_model`, `state.feedback_blocks`,
/// `state.focus`, `state.needs_redraw`). Called directly from
/// `handle_apply_model_switch` (intra-module) AND from the dispatch arm after
/// a `RequestSpawn::HealthCheck` task completes successfully.
pub async fn handle_complete_model_switch(
    state: &mut TuiState,
    info: &dyn ProviderInfoPort,
    resolved_pid: &str,
    model_id: &str,
) -> HandlerOutcome {
    use crate::adapters::tui::widgets::model_selector::humanize_ctx;

    if resolved_pid != info.active_delegate_id() {
        if let Err(e) = info.set_active_provider(resolved_pid) {
            notifications::warning(state, NoticeSource::Model, format!("Switch failed: {}", e));
            state.needs_redraw = true;
            return HandlerOutcome::Quiet;
        }
    }

    state.selected_model = Some(model_id.to_string());

    let model_desc = info.get_model(resolved_pid, model_id);
    let context_window = model_desc.as_ref().map_or(0, |m| m.context_window);
    let provider_display_name = info
        .list_providers()
        .into_iter()
        .find(|p| p.provider_id == resolved_pid)
        .map(|p| p.display_name)
        .unwrap_or_else(|| resolved_pid.to_string());
    let model_display_name = model_desc
        .as_ref()
        .map(|m| m.display_name.clone())
        .unwrap_or_else(|| model_id.to_string());

    notifications::success(
        state,
        NoticeSource::Model,
        format!(
            "Switched to {}/{} (context: {})",
            provider_display_name,
            model_display_name,
            humanize_ctx(context_window)
        ),
    );

    // Story 7.3 AC7: warn when switching to a non-tool model
    if let Some(ref md) = model_desc {
        use crate::domain::models::provider::ModelCapability;
        if !md.capabilities.contains(&ModelCapability::ToolUse) {
            notifications::warning(
                state,
                NoticeSource::Model,
                format!(
                    "{} does not support tool use. Tool execution will be unavailable.",
                    md.display_name
                ),
            );
        }
    }

    reducer::close(state, Surface::ModelSelector);
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}

/// One-shot startup convenience: if the active provider is unhealthy, fall back
/// to the first healthy registered provider (deterministic — sorted).
/// Returns:
/// - `HandlerOutcome::Quiet` when the active provider is healthy (no fallback needed)
/// - `HandlerOutcome::Notify(SystemNotice)` when fallback engaged OR no provider reachable
///
/// The `Notify` event is routed via `event_bus.emit_domain(...)` at the dispatch
/// arm (NOT via `domain_tx.send`) — preserving the original `app_state.event_bus.emit_domain`
/// pattern which was already conformant.
pub async fn handle_apply_startup_provider_fallback(
    state: &mut TuiState,
    info: &dyn ProviderInfoPort,
) -> HandlerOutcome {
    let active = info.active_delegate_id();
    let providers = info.list_providers();
    let active_healthy = providers
        .iter()
        .find(|p| p.provider_id == active)
        .is_some_and(|p| p.healthy);

    if active_healthy {
        return HandlerOutcome::Quiet;
    }

    let mut healthy_ids: Vec<String> = providers
        .iter()
        .filter(|p| p.healthy)
        .map(|p| p.provider_id.clone())
        .collect();
    healthy_ids.sort();

    if let Some(healthy_id) = healthy_ids.into_iter().next() {
        if let Err(e) = info.set_active_provider(&healthy_id) {
            tracing::warn!("Failed to set active provider to '{}': {}", healthy_id, e);
            return HandlerOutcome::Quiet;
        }
        if let Some(first_model) = info.list_models_by_provider(&healthy_id).first() {
            state.selected_model = Some(first_model.model_id.clone());
        }
        HandlerOutcome::Notify(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: format!(
                "Active provider '{}' unavailable — using '{}'.",
                active, healthy_id
            ),
        })
    } else {
        HandlerOutcome::Notify(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: if info.list_locked_providers().is_empty() {
                format!(
                    "No provider is reachable. Start a provider (e.g. `ollama serve`) or open the model selector with {}.",
                    crate::adapters::tui::keymap::render::leader_follow_up_long(
                        crate::adapters::tui::keymap::table::LeaderCmd::Model
                    )
                )
            } else {
                format!(
                    "No provider is reachable. Sign in with /login or `rustain auth login <provider>`, or open the model selector ({}) to see locked providers.",
                    crate::adapters::tui::keymap::render::leader_follow_up_long(
                        crate::adapters::tui::keymap::table::LeaderCmd::Model
                    )
                )
            },
        })
    }
}
