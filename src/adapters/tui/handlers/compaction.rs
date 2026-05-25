//! Compaction handlers — Story 7.4. **Phase 2 Task 3 prototype (load-bearing
//! for ADR-08-01 §D2 spawn-stays invariant).**
//!
//! Handles `AppEvent` family (consumed via `event_loop.rs` dispatch arms):
//! - `AppEvent::CompactionComplete` — emitted by `run_compaction` spawn body
//! - `AppEvent::CompactionFailed` — emitted by `run_compaction` spawn body
//! - `AppEvent::SystemNotice` — emitted inline by `handle_trigger_compaction`
//!   guards (preserves 3 `CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS` tags from
//!   pre-extraction; DGI-D / Winston Task 1 sign-off 2026-05-16)
//!
//! Per ADR-08-01 §D2: the `tokio::spawn(...)` for the compaction task lives
//! at the dispatch site (event_loop.rs) — NOT in this module. The handler
//! returns `HandlerOutcome::RequestCompaction(CompactionPayload)` and the
//! dispatch arm constructs the spawn via `tokio::spawn(run_compaction(payload))`.
//!
//! Per Story 8.0a Phase 2 design (user direction "per spec + long-term
//! correctness"): `handle_trigger_compaction` takes `&dyn ProviderInfoPort`
//! (domain port from `domain/ports/provider_info.rs`) instead of `&AppState` +
//! `&ProviderRouter` to satisfy AC-5 domain isolation. Dispatch arm constructs
//! `AppContext::new(&app_state, &router)` and passes it as the port.

#![allow(dead_code)] // dispatch arms wired in this Phase 2 prototype task

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::adapters::tui::state::TuiState;
use crate::domain::events::{AppEvent, CompactionPurpose};
use crate::domain::models::{AppConfig, Conversation, NoticeLevel, StreamChunk, StreamingState};
use crate::domain::ports::{ProviderInfoPort, StreamingProvider};
use crate::domain::services::compaction::first_kept_message_id;

use super::{CompactionPayload, HandlerOutcome};

/// Background task timeout for compaction summary generation. Mirrors
/// `event_loop.rs::BACKGROUND_TASK_TIMEOUT` (kept duplicate at extraction time
/// per anti-scope "do not introduce new abstractions" — Phase 4 may consolidate).
pub(super) const COMPACTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Effective model id: `state.selected_model` if set, else `config.model`.
/// Co-located with the compaction handlers since both helpers in this module
/// need it. Mirrors `event_loop.rs::effective_model` (duplicate kept for now;
/// Phase 4 may consolidate into `handlers/shared.rs` if a second consumer emerges).
fn effective_model<'a>(state: &'a TuiState, config: &'a AppConfig) -> &'a str {
    state.selected_model.as_deref().unwrap_or(&config.model)
}

/// Guards + payload construction for compaction. Inline `domain_tx.send`
/// for guard notices preserves existing `CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS`
/// tags per DGI-D / Winston Task 1 sign-off (2026-05-16).
///
/// Returns either:
/// - `HandlerOutcome::Quiet` if a guard tripped (notice already emitted inline)
/// - `HandlerOutcome::RequestCompaction(payload)` if conditions allow spawning
///
/// Dispatch-arm pattern:
/// ```ignore
/// let app_context = AppContext::new(&app_state, &router);
/// match handlers::compaction::handle_trigger_compaction(
///     &conversation, &streaming, &mut state, &provider, config, &domain_tx,
///     CompactionPurpose::Inline, &app_context,
/// ) {
///     HandlerOutcome::Quiet => {}
///     HandlerOutcome::RequestCompaction(payload) => {
///         tokio::spawn(handlers::compaction::run_compaction(payload));
///     }
///     _ => unreachable!("compaction handler only returns Quiet or RequestCompaction"),
/// }
/// ```
#[allow(clippy::too_many_arguments)]
pub fn handle_trigger_compaction(
    conversation: &Conversation,
    streaming: &StreamingState,
    state: &mut TuiState,
    provider: &Arc<dyn StreamingProvider>,
    config: &AppConfig,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
    purpose: CompactionPurpose,
    info: &dyn ProviderInfoPort,
) -> HandlerOutcome {
    if streaming.is_streaming || state.compacting {
        let event = AppEvent::SystemNotice {
            conversation_id: Some(conversation.id.clone()),
            level: NoticeLevel::Info,
            message: "Compaction unavailable while a turn is in progress.".to_string(),
        };
        let _ = domain_tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: guard notice before async spawn
        return HandlerOutcome::Quiet;
    }

    let first_kept = match &purpose {
        CompactionPurpose::Inline | CompactionPurpose::SwitchAfter { .. } => {
            match first_kept_message_id(conversation) {
                Some(id) => Some(id),
                None => {
                    let event = AppEvent::SystemNotice {
                        conversation_id: Some(conversation.id.clone()),
                        level: NoticeLevel::Info,
                        message: "Not enough conversation history to compact.".to_string(),
                    };
                    let _ = domain_tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: guard notice before async spawn
                    return HandlerOutcome::Quiet;
                }
            }
        }
        CompactionPurpose::Carryover => None,
    };

    // Resolve context-window via the port (domain isolation: no &AppState/&ProviderRouter import).
    let active_cw = info
        .get_model(&info.active_delegate_id(), effective_model(state, config))
        .map_or(0u32, |m| m.context_window);

    let pre_tokens = conversation.usage.as_ref().map_or(0, |u| u.input_tokens);
    let history_text = match &purpose {
        CompactionPurpose::Inline => {
            crate::domain::services::compaction::build_compaction_prompt_input(
                conversation,
                first_kept.as_deref().unwrap_or(""),
                active_cw.max(1),
            )
        }
        CompactionPurpose::SwitchAfter {
            provider_id,
            model_id,
        } => {
            // Budget the summary against the target model's context window (team decision)
            let target_cw = info
                .get_model(provider_id, model_id)
                .map_or(0u32, |m| m.context_window);
            crate::domain::services::compaction::build_compaction_prompt_input(
                conversation,
                first_kept.as_deref().unwrap_or(""),
                target_cw.max(1),
            )
        }
        CompactionPurpose::Carryover => {
            // Summarize entire conversation for carryover (empty boundary => boundary = messages.len())
            crate::domain::services::compaction::build_compaction_prompt_input(
                conversation,
                "",
                active_cw.max(1),
            )
        }
    };

    state.compacting = true;
    let event = AppEvent::SystemNotice {
        conversation_id: Some(conversation.id.clone()),
        level: NoticeLevel::Info,
        message: "Compacting context…".to_string(),
    };
    let _ = domain_tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: guard notice before async spawn

    HandlerOutcome::RequestCompaction(CompactionPayload {
        provider: provider.clone(),
        model: effective_model(state, config).to_string(),
        history_text,
        conversation_id: conversation.id.clone(),
        first_kept_message_id: first_kept,
        pre_tokens,
        purpose,
        domain_tx: domain_tx.clone(),
    })
}

/// Spawn-body helper — runs the compaction summary generation and emits the
/// terminal event (`AppEvent::CompactionComplete` or `AppEvent::CompactionFailed`)
/// via the carried `domain_tx`. Called from the dispatch arm via
/// `tokio::spawn(run_compaction(payload))` per ADR-08-01 §D2.
///
/// Returns `impl Future<Output = ()>` (as `async fn`) — NOT a HandlerOutcome.
/// This helper is the spawn body, not a handler.
pub async fn run_compaction(payload: CompactionPayload) {
    let CompactionPayload {
        provider,
        model,
        history_text,
        conversation_id,
        first_kept_message_id,
        pre_tokens,
        purpose,
        domain_tx,
    } = payload;

    let result = tokio::time::timeout(
        COMPACTION_TIMEOUT,
        generate_compaction_summary(&*provider, &model, &history_text),
    )
    .await;

    match result {
        Ok(Ok(summary)) => {
            let event = AppEvent::CompactionComplete {
                conversation_id,
                summary,
                first_kept_message_id,
                pre_tokens,
                purpose,
            };
            let _ = domain_tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: async round-trip pattern (mirrors TitleGenerated)
        }
        Ok(Err(e)) => {
            let event = AppEvent::CompactionFailed {
                conversation_id,
                reason: format!("{}", e),
                purpose,
            };
            let _ = domain_tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: async round-trip pattern (mirrors TitleGenerated)
        }
        Err(_) => {
            let event = AppEvent::CompactionFailed {
                conversation_id,
                reason: "Compaction timed out".to_string(),
                purpose,
            };
            let _ = domain_tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: async round-trip pattern (mirrors TitleGenerated)
        }
    }
}

/// Generate a compaction summary for a conversation using the LLM provider.
/// Private to this module — extracted from `event_loop.rs::generate_compaction_summary`.
async fn generate_compaction_summary(
    provider: &dyn StreamingProvider,
    model: &str,
    history_text: &str,
) -> anyhow::Result<String> {
    use crate::domain::models::{CompletionOptions, Message, MessageRole as MsgRole};

    let messages = vec![Message {
        role: MsgRole::User,
        content: history_text.to_string(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
    }];
    let options = CompletionOptions {
        model: model.to_string(),
        max_tokens: 2048,
        system_prompt: crate::domain::services::compaction::COMPACTION_SYSTEM_PROMPT.to_string(),
        temperature: None,
        tools: vec![],
    };

    let stream = provider.stream_completion(messages, options).await?;
    let summary = crate::domain::services::streaming_collect::collect_text(stream).await?;
    if summary.is_empty() {
        anyhow::bail!("Compaction summary produced empty result");
    }
    Ok(summary)
}

// `collect_text_chunks` consolidated to `domain/services/streaming_collect::collect_text`
// per Story 8.0a Phase 4 (Winston Decision Gate amendment 2026-05-17).
