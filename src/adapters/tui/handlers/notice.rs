//! Density transition helper (Story 21.1 AC4).
//!
//! The Focus-mode notification queue, the warning-block helper, the background
//! stash and the Monitor auto-switch were deleted by Story 21.1: every notice is
//! now a `notifications` store record that paints as a TTL toast in every
//! density mode. Only the density transition itself remains here.

#![allow(dead_code)]

use crate::adapters::tui::state::TuiState;

/// Apply density mode transition: update mode and the column's visibility. The column paints
/// the read-only monitor body and is never a focus target, so no focus changes (7-E: the
/// History sidebar panel it used to open and focus is the History window now).
/// Extracted from event_loop.rs dispatch arm per code review D-2 (AC-11 ≤10 LOC ratchet).
pub(crate) fn apply_density_transition(
    state: &mut TuiState,
    mode: crate::domain::models::visual::DensityMode,
    event_bus: &crate::infrastructure::runtime::event_bus::EventBus,
) {
    state.density_mode = mode;
    state.density_user_overridden = true;
    state.sidebar_visible = mode.default_sidebar_visible();
    let _ = event_bus.emit_domain(crate::domain::events::AppEvent::SystemNotice {
        conversation_id: None,
        level: crate::domain::models::NoticeLevel::Info,
        message: format!("Density: {}", mode.display_label()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::notifications::{NoticeSeverity, NoticeSource, notify};
    use crate::domain::models::FocusState;
    use crate::domain::models::visual::DensityMode;

    #[test]
    fn story21_1_warning_notice_does_not_transfer_focus() {
        let mut state = TuiState::with_capability(
            80,
            24,
            crate::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        state.focus = FocusState::Input;
        notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Warning,
            "Auto-skipped task 3",
        );
        assert_eq!(
            state.focus,
            FocusState::Input,
            "Warning notice must not transfer focus from Input"
        );
        assert_eq!(state.notifications.visible().count(), 1);
    }

    #[test]
    fn story21_1_density_transition_never_holds_back_notices() {
        // Leaving Focus used to drain a queue into blocks; there is no queue now.
        let mut state = TuiState::with_capability(
            80,
            24,
            crate::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        assert_eq!(state.density_mode, DensityMode::Focus);
        notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Info,
            "visible in Focus",
        );
        assert_eq!(state.notifications.visible().count(), 1);
        let (bus, _rx) = crate::infrastructure::runtime::event_bus::EventBus::new(8);
        apply_density_transition(&mut state, DensityMode::Monitor, &bus);
        assert_eq!(state.notifications.visible().count(), 1);
        assert!(state.feedback_blocks.is_empty());
    }
}
