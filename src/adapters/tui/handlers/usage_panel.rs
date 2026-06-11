//! Usage-panel handler — Story 7.5 AC3.
//!
//! `handle_open_usage_panel` (formerly `open_usage_panel`) is an async
//! `HandlerOutcome::Quiet` handler — heavy async I/O (reads via
//! `UsageLedgerPort`), mutates `state.usage_panel` + `state.focus` +
//! `state.needs_redraw`. No event emission, no spawn.
//!
//! Takes TWO domain ports: `&dyn UsageLedgerPort` (for ledger reads) +
//! `&dyn ProviderInfoPort` (for active provider lookup + model metadata
//! to populate `context_window_tokens`). Dispatch arm provides both:
//! `&*app_state.usage_ledger` + `&app_context`.

#![allow(dead_code)]

use crate::adapters::tui::state::{SessionUsageSummary, TuiState, TurnUsageRow};
use crate::domain::models::{AppConfig, Conversation, MessageRole, SessionManager, SessionState};
use crate::domain::ports::{ProviderInfoPort, UsageLedgerPort};
use crate::domain::services::cost_calculator;

use super::HandlerOutcome;

/// Story 7.5 AC3: build the usage-panel state from today's ledger entries.
/// Pure-ish: only I/O is `read_session` + `read_since` on the usage ledger.
pub async fn handle_open_usage_panel(
    state: &mut TuiState,
    conversation: &Conversation,
    usage_ledger: &dyn UsageLedgerPort,
    info: &dyn ProviderInfoPort,
    config: &AppConfig,
    session_manager: &SessionManager,
) -> HandlerOutcome {
    let since = info.today_start_unix_ms();
    let entries_today = usage_ledger.read_since(since).await.unwrap_or_default();

    // Per-session entries for the current session (for turn-row join)
    let session_id_opt = match session_manager.state() {
        SessionState::Active { id } => Some(id.clone()),
        _ => None,
    };
    let entries_session = match &session_id_opt {
        Some(sid) => usage_ledger.read_session(sid).await.unwrap_or_default(),
        None => Vec::new(),
    };

    let breakdown = cost_calculator::cost_breakdown(&entries_today, &config.pricing);
    let total_in: u64 = entries_today.iter().map(|e| e.usage.tokens_in as u64).sum();
    let total_out: u64 = entries_today
        .iter()
        .map(|e| e.usage.tokens_out as u64)
        .sum();
    let cache_read_today: u64 = entries_today
        .iter()
        .map(|e| e.usage.cache_read_tokens.unwrap_or(0) as u64)
        .sum();
    let cache_creation_today: u64 = entries_today
        .iter()
        .map(|e| e.usage.cache_creation_tokens.unwrap_or(0) as u64)
        .sum();
    let cache_total = cache_read_today + cache_creation_today + total_in;
    let cache_savings_usd = cost_calculator::cache_savings(&entries_today, &config.pricing);

    let task_count = conversation
        .turns
        .iter()
        .filter(|t| t.role == MessageRole::Assistant)
        .count() as u32;
    let elapsed_secs = (info.now_unix() - conversation.created_at.max(0)).max(0);

    // Build per-turn rows by joining each Assistant turn to its closest ledger entry.
    let mut turn_rows: Vec<TurnUsageRow> = Vec::new();
    for (idx, turn) in conversation
        .turns
        .iter()
        .enumerate()
        .filter(|(_, t)| t.role == MessageRole::Assistant)
    {
        let matched: Option<&crate::domain::models::usage::UsageLedgerEntry> = entries_session
            .iter()
            .filter(|e| {
                e.conversation_id == conversation.id
                    && (turn.model.is_empty() || e.model == turn.model)
            })
            .min_by_key(|e| (e.timestamp_ms - turn.started_at).abs());
        let (tokens_in, tokens_out, model, cost_usd) = if let Some(e) = matched {
            (
                e.usage.tokens_in,
                e.usage.tokens_out,
                e.model.clone(),
                cost_calculator::cost_for_entry(e, &config.pricing),
            )
        } else {
            (
                0,
                0,
                if turn.model.is_empty() {
                    effective_model(state, config).to_string()
                } else {
                    turn.model.clone()
                },
                None,
            )
        };
        turn_rows.push(TurnUsageRow {
            turn_index: idx as u32,
            model,
            tokens_in,
            tokens_out,
            cost_usd,
        });
    }

    let panel_session_today = SessionUsageSummary {
        tokens_in: total_in,
        tokens_out: total_out,
        cost_usd: if entries_today.is_empty() {
            None
        } else {
            Some(breakdown.total_usd)
        },
        task_count,
        elapsed_secs,
        cache_read_tokens: cache_read_today,
        cache_total_tokens: cache_total,
        cache_savings_usd,
    };

    // Snapshot context window for the active model via the port.
    let token_usage_in = conversation.usage.as_ref().map_or(0u32, |u| {
        u.input_tokens
            .saturating_add(u.cache_creation_input_tokens.unwrap_or(0))
            .saturating_add(u.cache_read_input_tokens.unwrap_or(0))
    });
    let context_window_tokens = info
        .get_model(&info.active_delegate_id(), effective_model(state, config))
        .map_or(0u32, |m| m.context_window);

    state.usage_panel.turn_rows = turn_rows;
    state.usage_panel.session_today = panel_session_today;
    state.usage_panel.per_model = breakdown.per_model;
    state.usage_panel.missing_pricing_models = breakdown.missing_pricing_models;
    state.usage_panel.context_used_tokens = token_usage_in;
    state.usage_panel.context_window_tokens = context_window_tokens;
    state.usage_panel.open(state.focus.clone());
    state.focus = crate::domain::models::FocusState::Overlay(
        crate::domain::models::visual::OverlayType::UsagePanel,
    );
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}

/// Effective model id: `state.selected_model` if set, else `config.model`.
/// Duplicate of `event_loop.rs::effective_model` + `handlers/compaction.rs::effective_model`.
/// Phase 4 cleanup candidate: consolidate to `handlers/shared.rs` when 3rd consumer settles.
fn effective_model<'a>(state: &'a TuiState, config: &'a AppConfig) -> &'a str {
    state.selected_model.as_deref().unwrap_or(&config.model)
}
