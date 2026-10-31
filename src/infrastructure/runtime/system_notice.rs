//! `AppEvent::SystemNotice` consumer effects, extracted from `event_loop::run`
//! so the production caller and the K2 keystone drive the same code (Story 21.2
//! AC2).
//!
//! Only a turn-fatal notice (`NoticeLevel::is_turn_fatal`: Warning/Error) resets
//! streaming state and cancels the active turn. `Advisory` — the level of an
//! input-validation notice such as an unknown slash command — is rendered with
//! the same warning row but leaves the live stream, its cancellation token and
//! its tool blocks untouched.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::adapters::tui::handlers;
use crate::adapters::tui::state::TuiState;
use crate::domain::models::tab::TabManager;
use crate::domain::models::{
    Conversation, FeedbackAction, FeedbackBlock, FeedbackLevel, FocusState, NoticeLevel,
    StatusState, StreamingPhase, StreamingState,
};
use crate::infrastructure::runtime::event_bus::EventBus;
use crate::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle;

/// Apply a notice addressed to the ACTIVE conversation (or a global one).
pub(crate) fn apply_active_notice(
    state: &mut TuiState,
    streaming: &mut StreamingState,
    tabs: &mut TabManager,
    conversation: &mut Conversation,
    lifecycle: &mut TuiTurnLifecycle,
    event_bus: &EventBus,
    level: NoticeLevel,
    msg: String,
) {
    // Only turn-fatal notices reset streaming and abort the turn. `Advisory`
    // is warning-class but NOT fatal (story 19.2 review): a disclosure — or a
    // rejected input — must never cancel the turn it was raised beside.
    if level.is_turn_fatal() {
        streaming.is_streaming = false;
        streaming.phase = StreamingPhase::Idle;
        streaming.current_blocks.clear();
        streaming.active_tool_calls.clear();
        lifecycle.interrupt_active(
            tabs,
            conversation,
            streaming,
            &mut state.pending_resolved_model,
        );
    }

    // Story 13-2 AC5: rewrite offline provider errors with user-friendly copy
    // instead of raw error strings.
    let msg = if matches!(level, NoticeLevel::Error) && msg.starts_with("Offline:") {
        "✗ No provider available. Configure a local LLM for offline use, or check your connection."
            .to_string()
    } else {
        msg
    };

    // FeedbackBlock for errors and warnings; flash for info.
    match level {
        NoticeLevel::Error => {
            static FB_COUNTER: AtomicUsize = AtomicUsize::new(0);
            let fb_id = format!("fb-{}", FB_COUNTER.fetch_add(1, Ordering::Relaxed));
            let fb = FeedbackBlock {
                id: fb_id.clone(),
                level: FeedbackLevel::Error,
                message: msg,
                actions: vec![FeedbackAction::Retry],
            };
            state.feedback_blocks.insert(fb_id.clone(), fb);
            state.active_feedback_id = Some(fb_id);
            state.status = StatusState::Idle;
            state.focus = FocusState::Chat;
            handlers::notice::auto_switch_to_monitor_on_error(state, event_bus);
        }
        NoticeLevel::Warning | NoticeLevel::Advisory => {
            handlers::notice::apply_warning_notice(state, msg);
        }
        _ => {
            let flash_duration = state.theme.timing.status_flash_ms;
            handlers::notice::notify_or_queue(
                state,
                crate::adapters::tui::state::QueuedNotification::StatusFlash {
                    level,
                    message: msg,
                    duration_ms: flash_duration,
                },
            );
        }
    }
    state.needs_redraw = true;
}

/// Apply a notice addressed to a BACKGROUND tab: its stored streaming state
/// ends only for a turn-fatal level, and the notice survives the switch.
pub(crate) fn apply_background_notice(
    state: &mut TuiState,
    tabs: &mut TabManager,
    conversation_id: &str,
    level: NoticeLevel,
    msg: String,
) {
    let Some(tab) = tabs.find_by_conversation_mut(conversation_id) else {
        return;
    };
    if level.is_turn_fatal() {
        tab.streaming.is_streaming = false;
        tab.streaming.phase = StreamingPhase::Idle;
        tab.streaming.current_blocks.clear();
        tab.streaming.active_tool_calls.clear();
    }
    // Errors were always stored; a Warning/Advisory notice addressed here (e.g.
    // a late `/team send` reply) must survive the switch too.
    handlers::notice::store_background_notice(tab, level, msg);
    // Redraw so the tab bar can reflect the state change.
    state.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    struct Fixture {
        state: TuiState,
        streaming: StreamingState,
        tabs: TabManager,
        conversation: Conversation,
        lifecycle: TuiTurnLifecycle,
        bus: EventBus,
        token: CancellationToken,
    }

    /// A live controlled stream: chunk A has been applied, the producer is
    /// registered with the lifecycle under the real conversation id.
    fn live_stream() -> Fixture {
        let tabs = TabManager::new(CancellationToken::new());
        let conversation = tabs.active_tab().conversation.clone();
        let mut lifecycle = TuiTurnLifecycle::default();
        let token = CancellationToken::new();
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        lifecycle.register(
            conversation.id.clone(),
            token.clone(),
            "m".into(),
            domain_tx,
        );
        let mut streaming = StreamingState::default();
        streaming.is_streaming = true;
        streaming.phase = StreamingPhase::AccumulatingText;
        streaming.current_text_buffer = "chunk A".into();
        Fixture {
            state: TuiState::new(80, 24),
            streaming,
            tabs,
            conversation,
            lifecycle,
            bus: EventBus::new(8).0,
            token,
        }
    }

    fn apply(f: &mut Fixture, level: NoticeLevel, text: &str) {
        apply_active_notice(
            &mut f.state,
            &mut f.streaming,
            &mut f.tabs,
            &mut f.conversation,
            &mut f.lifecycle,
            &f.bus,
            level,
            text.to_owned(),
        );
    }

    #[tokio::test]
    async fn story21_2_advisory_unknown_slash_keeps_the_live_stream() {
        let mut f = live_stream();
        let blocks_before = f.streaming.current_blocks.len();
        apply(
            &mut f,
            NoticeLevel::Advisory,
            "Unknown command: /nope — not a built-in or a custom command.",
        );
        assert!(f.streaming.is_streaming, "stream keeps running");
        assert_eq!(
            f.streaming.current_text_buffer, "chunk A",
            "chunk A retained"
        );
        assert_eq!(f.streaming.current_blocks.len(), blocks_before);
        assert!(!f.token.is_cancelled(), "same live cancellation token");
        assert!(
            f.lifecycle.is_live(&f.conversation.id),
            "same live producer"
        );
        // The notice is visible as a warning-class block (or queued for it).
        assert!(f.state.needs_redraw);
    }

    #[tokio::test]
    async fn story21_2_warning_and_error_notices_still_stop_the_turn() {
        for level in [NoticeLevel::Warning, NoticeLevel::Error] {
            let mut f = live_stream();
            apply(&mut f, level, "provider failed");
            assert!(!f.streaming.is_streaming, "{level:?} is turn-fatal");
            assert!(f.token.is_cancelled(), "{level:?} cancels the producer");
            assert!(!f.lifecycle.is_live(&f.conversation.id));
        }
    }

    #[tokio::test]
    async fn story21_2_background_advisory_leaves_the_owned_tab_stream_alone() {
        let mut f = live_stream();
        f.tabs.active_tab_mut().streaming.is_streaming = true;
        let id = f.conversation.id.clone();
        apply_background_notice(
            &mut f.state,
            &mut f.tabs,
            &id,
            NoticeLevel::Advisory,
            "unknown".into(),
        );
        assert!(f.tabs.active_tab().streaming.is_streaming);
        apply_background_notice(
            &mut f.state,
            &mut f.tabs,
            &id,
            NoticeLevel::Warning,
            "fatal".into(),
        );
        assert!(
            !f.tabs.active_tab().streaming.is_streaming,
            "fatal notice ends it"
        );
    }
}
