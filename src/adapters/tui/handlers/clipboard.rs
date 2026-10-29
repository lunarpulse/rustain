//! The one copy-to-clipboard flash every copy key shares.

use crate::adapters::tui::clipboard::ClipboardResult;
use crate::adapters::tui::state::TuiState;
use crate::domain::models::StatusState;

/// Shows `message` as a status flash, remembering the status it replaces.
pub(crate) fn flash(state: &mut TuiState, message: String) {
    state.status_before_flash = Some(state.status.clone());
    state.status = StatusState::Flash {
        message,
        remaining_ms: state.theme.timing.status_flash_ms,
    };
    state.needs_redraw = true;
}

/// Copies `text` through `copy` (production: `clipboard::copy_to_clipboard`)
/// and flashes the outcome; empty text flashes `Nothing to copy`.
pub(crate) fn flash_copy(state: &mut TuiState, text: &str, copy: &dyn Fn(&str) -> ClipboardResult) {
    let message = if text.is_empty() {
        "Nothing to copy".to_string()
    } else {
        match copy(text) {
            ClipboardResult::Osc52Success => "Copied to clipboard".to_string(),
            ClipboardResult::FallbackSuccess(path) => format!("Copied to {}", path.display()),
            ClipboardResult::Failed(error) => format!("Copy failed: {error}"),
        }
    };
    flash(state, message);
}
