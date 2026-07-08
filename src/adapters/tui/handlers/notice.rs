//! Warning-notice helper — cross-epic utility.
//!
//! `apply_warning_notice` is a `pub(crate)` helper per DGI-A (returns `String`
//! — the feedback-block ID — NOT `HandlerOutcome`). Called by warning paths
//! from other handlers + event_loop.rs dispatch arms.
//!
//! DGI-E: the inline `#[cfg(test)] mod tests` from `event_loop.rs::warning_notice_does_not_transfer_focus`
//! was moved alongside the helper during this extraction per Winston Task 1 sign-off.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::adapters::tui::state::TuiState;
use crate::domain::models::{FeedbackAction, FeedbackBlock, FeedbackLevel};

use crate::adapters::tui::state::QueuedNotification;

/// Route a notification through the density-mode queue or apply it directly.
/// Focus mode: enqueues to bounded VecDeque (cap 32; oldest dropped on overflow).
/// Other modes: applies immediately (status flash or feedback block insertion).
/// Story 8.4b AC-8.
pub(crate) fn notify_or_queue(state: &mut TuiState, kind: QueuedNotification) {
    use crate::domain::models::visual::DensityMode;
    if state.density_mode == DensityMode::Focus {
        if state.queued_notifications.len() >= 32 {
            tracing::warn!(
                dropped = ?state.queued_notifications.front(),
                "Focus-mode notification queue full; dropping oldest"
            );
            state.queued_notifications.pop_front();
        }
        state.queued_notifications.push_back(kind);
    } else {
        match kind {
            QueuedNotification::StatusFlash {
                level: _level,
                message,
                duration_ms,
            } => {
                state.status_before_flash = Some(state.status.clone());
                state.status = crate::domain::models::StatusState::Flash {
                    message,
                    remaining_ms: duration_ms,
                };
            }
            QueuedNotification::FeedbackBlock { id, level, message } => {
                let fb = crate::domain::models::FeedbackBlock {
                    id: id.clone(),
                    level,
                    message,
                    actions: vec![FeedbackAction::Dismiss],
                };
                state.feedback_blocks.insert(id, fb);
            }
        }
    }
}

/// Apply a warning-level notice to TUI state. Creates a FeedbackBlock,
/// sets `active_feedback_id`, and returns the block ID. Does NOT mutate
/// focus — regression guard for AC6.
pub(crate) fn apply_warning_notice(state: &mut TuiState, msg: String) -> String {
    static WFB_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let fb_id = format!("wfb-{}", WFB_COUNTER.fetch_add(1, Ordering::Relaxed));
    notify_or_queue(
        state,
        QueuedNotification::FeedbackBlock {
            id: fb_id.clone(),
            level: FeedbackLevel::Warning,
            message: msg,
        },
    );
    // Set active_feedback_id only when the block was inserted directly (not queued)
    if state.density_mode != crate::domain::models::visual::DensityMode::Focus {
        state.active_feedback_id = Some(fb_id.clone());
    }
    fb_id
}

/// Apply density mode transition: update mode, reconcile sidebar, drain queue.
/// Extracted from event_loop.rs dispatch arm per code review D-2 (AC-11 ≤10 LOC ratchet).
pub(crate) fn apply_density_transition(
    state: &mut TuiState,
    mode: crate::domain::models::visual::DensityMode,
    event_bus: &crate::infrastructure::runtime::event_bus::EventBus,
) {
    use crate::domain::models::visual::DensityMode;

    let leaving_focus = state.density_mode == DensityMode::Focus;
    state.density_mode = mode;
    state.density_user_overridden = true;
    state.sidebar_visible = mode.default_sidebar_visible();
    if state.sidebar_visible {
        state.sidebar_panel = Some(crate::domain::models::visual::PanelType::Tasks);
        state.focus = crate::domain::models::FocusState::Sidebar {
            panel: crate::domain::models::visual::PanelType::Tasks,
            selected: state.sidebar_selected,
        };
    } else {
        state.sidebar_panel = None;
        state.focus = crate::domain::models::FocusState::Chat;
    }
    if leaving_focus {
        state.status_before_flash = Some(state.status.clone());
        while let Some(q) = state.queued_notifications.pop_front() {
            match q {
                QueuedNotification::StatusFlash {
                    level: _,
                    message,
                    duration_ms: _,
                } => {
                    let fb_id = {
                        static DRAIN_FB_COUNTER: std::sync::atomic::AtomicUsize =
                            std::sync::atomic::AtomicUsize::new(0);
                        format!(
                            "drain-fb-{}",
                            DRAIN_FB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        )
                    };
                    let fb = crate::domain::models::FeedbackBlock {
                        id: fb_id.clone(),
                        level: crate::domain::models::FeedbackLevel::Info,
                        message,
                        actions: vec![FeedbackAction::Dismiss],
                    };
                    state.feedback_blocks.insert(fb_id.clone(), fb);
                    state.active_feedback_id = Some(fb_id);
                }
                QueuedNotification::FeedbackBlock { id, level, message } => {
                    let fb = crate::domain::models::FeedbackBlock {
                        id: id.clone(),
                        level,
                        message,
                        actions: vec![FeedbackAction::Dismiss],
                    };
                    state.feedback_blocks.insert(id.clone(), fb);
                    state.active_feedback_id = Some(id);
                }
            }
        }
    }
    let _ = event_bus.emit_domain(crate::domain::events::AppEvent::SystemNotice {
        conversation_id: None,
        level: crate::domain::models::NoticeLevel::Info,
        message: format!("Density: {}", mode.display_label()),
    });
}

/// Auto-switch to Monitor mode on Error-level SystemNotice while in Focus.
/// Per party-mode consensus D-1 (per spec + long-term correctness): errors surface
/// immediately AND auto-switch mode so the status bar stays honest.
pub(crate) fn auto_switch_to_monitor_on_error(
    state: &mut TuiState,
    event_bus: &crate::infrastructure::runtime::event_bus::EventBus,
) {
    use crate::domain::models::visual::DensityMode;
    if state.density_mode == DensityMode::Focus {
        apply_density_transition(state, DensityMode::Monitor, event_bus);
        state.status_before_flash = Some(state.status.clone());
        state.status = crate::domain::models::StatusState::Flash {
            message: "Switched to Monitor — error requires attention".to_string(),
            remaining_ms: state.theme.timing.status_flash_ms,
        };
    }
}

/// Story 8.4b: drain the queued-notifications VecDeque (test helper).
#[cfg(test)]
pub(crate) fn drain_queued_notifications_for_test(state: &mut TuiState) -> Vec<QueuedNotification> {
    std::mem::take(&mut state.queued_notifications)
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::FocusState;

    #[test]
    fn warning_notice_does_not_transfer_focus() {
        let mut state = TuiState::with_capability(
            80,
            24,
            crate::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        state.focus = FocusState::Input;
        // Set to Monitor so apply_warning_notice inserts directly (not queued)
        state.density_mode = crate::domain::models::visual::DensityMode::Monitor;
        let fb_id = apply_warning_notice(&mut state, "Auto-skipped task 3".to_string());
        assert_eq!(
            state.focus,
            FocusState::Input,
            "Warning notice must not transfer focus from Input"
        );
        assert_eq!(
            state.active_feedback_id,
            Some(fb_id.clone()),
            "Warning notice must set active_feedback_id"
        );
        assert!(
            state.feedback_blocks.contains_key(&fb_id),
            "Warning notice must insert feedback block"
        );
    }

    // Story 8.4b notification queue tests

    #[test]
    fn notify_or_queue_queues_in_focus_mode() {
        let mut state = TuiState::with_capability(
            80,
            24,
            crate::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        state.density_mode = crate::domain::models::visual::DensityMode::Focus;
        let n = QueuedNotification::FeedbackBlock {
            id: "test-1".to_string(),
            level: FeedbackLevel::Warning,
            message: "queued message".to_string(),
        };
        notify_or_queue(&mut state, n);
        assert!(
            state.feedback_blocks.is_empty(),
            "Focus mode: no direct insert"
        );
        assert_eq!(state.queued_notifications.len(), 1);
    }

    #[test]
    fn notify_or_queue_inserts_directly_in_monitor() {
        let mut state = TuiState::with_capability(
            80,
            24,
            crate::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        state.density_mode = crate::domain::models::visual::DensityMode::Monitor;
        let n = QueuedNotification::FeedbackBlock {
            id: "test-2".to_string(),
            level: FeedbackLevel::Warning,
            message: "direct message".to_string(),
        };
        notify_or_queue(&mut state, n);
        assert!(
            state.feedback_blocks.contains_key("test-2"),
            "Monitor mode: direct insert"
        );
        assert!(state.queued_notifications.is_empty());
    }

    #[test]
    fn notify_or_queue_drops_oldest_on_overflow() {
        let mut state = TuiState::with_capability(
            80,
            24,
            crate::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        state.density_mode = crate::domain::models::visual::DensityMode::Focus;
        for i in 0..33 {
            let n = QueuedNotification::FeedbackBlock {
                id: format!("fb-{}", i),
                level: FeedbackLevel::Info,
                message: format!("msg {}", i),
            };
            notify_or_queue(&mut state, n);
        }
        assert_eq!(state.queued_notifications.len(), 32);
        // Oldest (fb-0) should have been dropped
        let first = state.queued_notifications.front().unwrap();
        match first {
            QueuedNotification::FeedbackBlock { id, .. } => {
                assert_eq!(id, "fb-1", "fb-0 was dropped; fb-1 is now oldest");
            }
            _ => panic!("expected feedback block"),
        }
    }

    #[test]
    fn queued_notifications_drain_in_fifo_order() {
        let mut state = TuiState::with_capability(
            80,
            24,
            crate::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        state.density_mode = crate::domain::models::visual::DensityMode::Focus;
        for i in 0..3 {
            let n = QueuedNotification::FeedbackBlock {
                id: format!("fb-{}", i),
                level: FeedbackLevel::Info,
                message: format!("msg {}", i),
            };
            notify_or_queue(&mut state, n);
        }
        let drained = drain_queued_notifications_for_test(&mut state);
        assert_eq!(drained.len(), 3);
        match &drained[0] {
            QueuedNotification::FeedbackBlock { id, .. } => assert_eq!(id, "fb-0"),
            _ => panic!("expected feedback block"),
        }
        match &drained[2] {
            QueuedNotification::FeedbackBlock { id, .. } => assert_eq!(id, "fb-2"),
            _ => panic!("expected feedback block"),
        }
    }
}
