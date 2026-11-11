//! Notice handling tests (Story 21.1 AC4).
//!
//! Every notice is a `notifications` store record that paints as a TTL toast. The Focus-mode
//! queue, the warning-block helper, the background stash, the Monitor auto-switch (21.1) and
//! the density transition (21.7, with the density model) are gone; only the focus rule stays
//! pinned here.

#[cfg(test)]
mod tests {
    use crate::adapters::tui::notifications::{NoticeSeverity, NoticeSource, notify};
    use crate::adapters::tui::state::TuiState;
    use crate::domain::models::FocusState;

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
}
