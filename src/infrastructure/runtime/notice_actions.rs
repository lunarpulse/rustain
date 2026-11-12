//! Local-host execution of notice actions and decision effects (Story 21.1
//! AC5, AC9, A8).
//!
//! S-LOG (`notification_log`) and the session decision queue (`decision_queue`)
//! decide *what* the user asked for; this module runs it with the local host's
//! facts. S-LOG `r`, the pane's Compact and the carryover decision share the
//! same two helpers, so there is one retry path and one compaction trigger.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::adapters::tui::handlers::{self, HandlerOutcome};
use crate::adapters::tui::notification_log::{LogFacts, LogOutcome};
use crate::adapters::tui::notifications::{self, NoticeAction, NoticeSource};
use crate::adapters::tui::state::TuiState;
use crate::domain::events::{AppEvent, CompactionPurpose};
use crate::domain::models::tab::TabManager;
use crate::domain::models::{
    AppConfig, Conversation, MessageRole, PermissionMode, RetryState, StatusState, StreamingState,
    next_delay,
};
use crate::domain::ports::StreamingProvider;
use crate::infrastructure::runtime::app_context::AppContext;
use crate::infrastructure::runtime::app_state::AppState;

/// Maximum automatic/manual retry attempts of the last user message.
const MAX_RETRY_ATTEMPTS: u8 = 5;

/// The host facts S-LOG needs to decide retry eligibility and name tabs.
#[must_use]
pub(crate) fn log_facts(
    conversation: &Conversation,
    streaming: &StreamingState,
    tab_manager: Option<&TabManager>,
    context_warning: bool,
) -> LogFacts {
    LogFacts {
        active_conversation_id: Some(conversation.id.clone()),
        active_messages: conversation.messages.len(),
        turn_running: streaming.is_streaming,
        retry_supported: true,
        decisions_supported: true,
        context_warning,
        titles: tab_manager
            .map(|tm| {
                tm.tabs()
                    .iter()
                    .map(|tab| (tab.conversation.id.clone(), tab.conversation.title.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The one notice every permission-mode change raises (A12). The status row
/// paints the YOLO fact itself from `permission_mode`, so YOLO is announced
/// exactly like the other modes — one Info notice, never a sticky status.
pub(crate) fn announce_permission_mode(state: &mut TuiState, mode: PermissionMode) {
    let label = match mode {
        PermissionMode::Plan => "Plan",
        PermissionMode::Normal => "Normal",
        PermissionMode::AutoEdit => "AutoEdit",
        PermissionMode::Yolo => "YOLO",
    };
    notifications::info(
        state,
        NoticeSource::Permission,
        format!("Permission mode: {label}"),
    );
    state.needs_redraw = true;
}

/// Re-send the last user message of `conversation` after the backoff delay:
/// bump the attempt counter, show `Retrying`, drop the failed assistant
/// response and schedule `AppEvent::RetryMessage`.
pub(crate) fn retry_last_turn(
    state: &mut TuiState,
    conversation: &mut Conversation,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let attempt = state.retry_state.as_ref().map_or(0, |r| r.attempt);
    if attempt >= MAX_RETRY_ATTEMPTS {
        notifications::warning(
            state,
            NoticeSource::System,
            "Stopped retrying because the retry limit was reached. Send your message again to try once more.",
        );
        state.retry_state = None;
        state.needs_redraw = true;
        return;
    }
    let delay = next_delay(attempt);
    state.retry_state = Some(RetryState {
        attempt: attempt + 1,
        max_attempts: MAX_RETRY_ATTEMPTS,
        delay_ms: delay,
    });
    state.status = StatusState::Retrying {
        attempt: attempt + 1,
        max: MAX_RETRY_ATTEMPTS,
        next_in_ms: delay,
    };
    state.needs_redraw = true;

    let Some(pos) = conversation
        .messages
        .iter()
        .rposition(|m| m.role == MessageRole::User)
    else {
        return;
    };
    let retry_text = conversation.messages[pos].content.clone();
    // Remove the failed assistant response after the last user message.
    conversation.messages.truncate(pos);
    let delay_duration = std::time::Duration::from_millis(delay);
    let tx = domain_tx.clone();
    let retry_conv_id = conversation.id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay_duration).await;
        let _ = tx.send(AppEvent::RetryMessage {
            conversation_id: retry_conv_id,
            content: retry_text,
        });
    });
}

/// Trigger a compaction of `conversation` for `purpose` (memory flush first):
/// `Inline` for Compact/Retry, `Carryover` for the carried-summary decision.
#[allow(clippy::too_many_arguments)]
pub(crate) fn trigger_compaction(
    state: &mut TuiState,
    conversation: &Conversation,
    streaming: &StreamingState,
    provider: &Arc<dyn StreamingProvider>,
    config: &AppConfig,
    purpose: CompactionPurpose,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
    app_state: &AppState,
    router: &crate::adapters::provider::ProviderRouter,
) {
    let app_context = AppContext::new(app_state, router);
    spawn_compaction(
        handlers::compaction::handle_trigger_compaction(
            conversation,
            streaming,
            state,
            provider,
            config,
            domain_tx,
            purpose,
            &app_context,
        ),
        app_state,
    );
}

/// Run the request a compaction trigger returned (memory flush first, then compact).
pub(crate) fn spawn_compaction(outcome: HandlerOutcome, app_state: &AppState) {
    match outcome {
        HandlerOutcome::Quiet => {}
        HandlerOutcome::RequestCompaction(payload) => {
            tokio::spawn(handlers::compaction::flush_then_compact(
                app_state.agent_core.memory.load_full().as_ref().clone(),
                payload,
            ));
        }
        _ => unreachable!("compaction handler only returns Quiet or RequestCompaction"),
    }
}

/// Daily-budget `Pause`: persist `dismissed_until_unix` = next local midnight
/// and suppress the warning until then (Story 7.5 AC5/AC7).
pub(crate) async fn budget_pause(
    state: &mut TuiState,
    app_state: &AppState,
    conversation_id: &str,
) {
    let next_midnight = crate::infrastructure::clock_util::next_midnight_unix();
    let new_state = crate::adapters::budget::BudgetState {
        dismissed_until_unix: next_midnight,
    };
    match app_state.budget_state_store.save(&new_state).await {
        Ok(()) => {
            super::decision_effects::apply_budget_pause(state, next_midnight);
            let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                conversation_id: Some(conversation_id.to_owned()),
                level: crate::domain::models::NoticeLevel::Info,
                message: "Budget warning paused until tomorrow.".to_string(),
            });
        }
        Err(e) => {
            tracing::error!("Failed to persist budget pause: {e}");
            let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                conversation_id: Some(conversation_id.to_owned()),
                level: crate::domain::models::NoticeLevel::Warning,
                message: "Couldn't persist budget pause state.".to_string(),
            });
        }
    }
    state.needs_redraw = true;
}

/// Execute what S-LOG asked for. `Consumed`/`Closed` need nothing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_log_outcome(
    outcome: LogOutcome,
    state: &mut TuiState,
    conversation: &mut Conversation,
    streaming: &StreamingState,
    provider: &Arc<dyn StreamingProvider>,
    config: &AppConfig,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
    app_state: &AppState,
    router: &crate::adapters::provider::ProviderRouter,
) {
    match outcome {
        LogOutcome::Consumed | LogOutcome::Closed | LogOutcome::Quit => {}
        LogOutcome::Copy(text) => handlers::clipboard::notify_copy(
            state,
            &text,
            &crate::adapters::tui::clipboard::copy_to_clipboard,
        ),
        LogOutcome::Retry {
            action,
            conversation_id,
        } => {
            if conversation_id != conversation.id {
                return;
            }
            match action {
                NoticeAction::RetryTurn { .. } => retry_last_turn(state, conversation, domain_tx),
                NoticeAction::RetryCompaction => trigger_compaction(
                    state,
                    conversation,
                    streaming,
                    provider,
                    config,
                    CompactionPurpose::Inline,
                    domain_tx,
                    app_state,
                    router,
                ),
                NoticeAction::ContextDecision => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::notifications::NoticeSeverity;
    use crate::domain::models::ChatMessage;

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            id: crate::domain::models::generate_conversation_id(),
            role,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin: crate::domain::models::ChannelKind::Terminal,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        }
    }

    // Paused runtime clock: the retry backoff elapses virtually (no real sleep).
    #[tokio::test(start_paused = true)]
    async fn story21_1_retry_last_turn_bumps_attempt_and_truncates_failed_response() {
        let mut state = TuiState::new(80, 24);
        let mut conversation = Conversation::default();
        conversation.messages.push(msg(MessageRole::User, "hello"));
        conversation
            .messages
            .push(msg(MessageRole::Assistant, "partial"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        retry_last_turn(&mut state, &mut conversation, &tx);
        assert_eq!(state.retry_state.as_ref().map(|r| r.attempt), Some(1));
        assert!(matches!(
            state.status,
            StatusState::Retrying { attempt: 1, .. }
        ));
        assert!(conversation.messages.is_empty());
        let delivered = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv()).await;
        match delivered.expect("retry delivered within the virtual backoff") {
            Some(AppEvent::RetryMessage { content, .. }) => assert_eq!(content, "hello"),
            other => panic!("expected RetryMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn story21_1_retry_last_turn_at_cap_notifies_instead_of_flashing() {
        let mut state = TuiState::new(80, 24);
        state.retry_state = Some(RetryState {
            attempt: MAX_RETRY_ATTEMPTS,
            max_attempts: MAX_RETRY_ATTEMPTS,
            delay_ms: 0,
        });
        let mut conversation = Conversation::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        retry_last_turn(&mut state, &mut conversation, &tx);
        assert!(state.retry_state.is_none());
        let record = state.notifications.visible().next().expect("a notice");
        assert_eq!(
            record.message,
            "Stopped retrying because the retry limit was reached. Send your message again to try once more."
        );
        assert_eq!(record.severity, NoticeSeverity::Warning);
    }

    #[test]
    fn story21_1_log_facts_name_every_tab() {
        let tab_manager = TabManager::new(tokio_util::sync::CancellationToken::new());
        let mut conversation = tab_manager.active_tab().conversation.clone();
        conversation.messages.push(msg(MessageRole::User, "hello"));
        conversation
            .messages
            .push(msg(MessageRole::Assistant, "answer"));
        let facts = log_facts(
            &conversation,
            &StreamingState::default(),
            Some(&tab_manager),
            false,
        );
        assert_eq!(
            facts.active_conversation_id.as_deref(),
            Some(conversation.id.as_str())
        );
        assert!(facts.retry_supported);
        assert_eq!(
            facts.active_messages, 2,
            "a turn retry is bound to the message count"
        );
        assert_eq!(facts.titles.len(), tab_manager.tab_count());
    }

    /// K6 — `announce_permission_mode`, the one call the event loop's
    /// `AppEvent::SetPermissionMode` arm makes (that arm itself is exercised by
    /// the native smoke only), announces every mode as an Info notice and never
    /// writes a sticky status; the YOLO fact is the status row's (`status_bar`
    /// K6 test), so leaving YOLO leaves no residue.
    #[test]
    fn story21_1_permission_mode_change_is_one_info_notice_and_no_status() {
        for (mode, text) in [
            (PermissionMode::Plan, "Permission mode: Plan"),
            (PermissionMode::Normal, "Permission mode: Normal"),
            (PermissionMode::AutoEdit, "Permission mode: AutoEdit"),
            (PermissionMode::Yolo, "Permission mode: YOLO"),
        ] {
            let mut state = TuiState::new(80, 24);
            state.status = StatusState::Idle;
            announce_permission_mode(&mut state, mode);
            let record = state.notifications.visible().next().expect("a notice");
            assert_eq!(record.message, text);
            assert_eq!(record.severity, NoticeSeverity::Info);
            assert_eq!(record.source, NoticeSource::Permission);
            assert_eq!(
                state.status,
                StatusState::Idle,
                "no sticky status for {mode:?}"
            );
        }
    }
}
