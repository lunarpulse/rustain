//! The one copy-to-clipboard notice every copy key shares (Story 21.1 AC4).

use crate::adapters::tui::clipboard::ClipboardResult;
use crate::adapters::tui::notifications::{NoticeSeverity, NoticeSource, notify};
use crate::adapters::tui::state::TuiState;

/// Copies `text` through `copy` (production: `clipboard::copy_to_clipboard`)
/// and reports the outcome as a notice: Success for a copy, Info for empty
/// text, Error for a failed copy (the typed result decides, never the text).
pub(crate) fn notify_copy(
    state: &mut TuiState,
    text: &str,
    copy: &dyn Fn(&str) -> ClipboardResult,
) {
    let (severity, message) = if text.is_empty() {
        (NoticeSeverity::Info, "Nothing to copy".to_string())
    } else {
        match copy(text) {
            ClipboardResult::Osc52Success => {
                (NoticeSeverity::Success, "Copied to clipboard".to_string())
            }
            ClipboardResult::FallbackSuccess(path) => (
                NoticeSeverity::Success,
                format!("Copied to {}", path.display()),
            ),
            ClipboardResult::Failed(error) => {
                (NoticeSeverity::Error, format!("Copy failed: {error}"))
            }
        }
    };
    notify(state, NoticeSource::Clipboard, severity, message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::StatusState;

    /// K6 — the clipboard family: the typed copy result picks the severity
    /// (never the text), and no outcome writes the status row.
    #[test]
    fn story21_1_copy_outcomes_are_typed_notices_and_never_a_status() {
        type Case = (&'static str, ClipboardResult, NoticeSeverity, &'static str);
        let cases: [Case; 4] = [
            (
                "x",
                ClipboardResult::Osc52Success,
                NoticeSeverity::Success,
                "Copied to clipboard",
            ),
            (
                "x",
                ClipboardResult::FallbackSuccess("/tmp/clip.txt".into()),
                NoticeSeverity::Success,
                "Copied to /tmp/clip.txt",
            ),
            (
                "x",
                ClipboardResult::Failed("no tty".into()),
                NoticeSeverity::Error,
                "Copy failed: no tty",
            ),
            (
                "",
                ClipboardResult::Osc52Success,
                NoticeSeverity::Info,
                "Nothing to copy",
            ),
        ];
        for (text, result, severity, message) in cases {
            let mut state = TuiState::new(80, 24);
            state.status = StatusState::Idle;
            let result = std::cell::Cell::new(Some(result));
            notify_copy(&mut state, text, &|_| {
                result.take().expect("one copy attempt")
            });
            let record = state.notifications.visible().next().expect("a notice");
            assert_eq!(
                (record.severity, record.message.as_str()),
                (severity, message)
            );
            assert_eq!(record.source, NoticeSource::Clipboard);
            assert_eq!(state.status, StatusState::Idle);
        }
    }
}
