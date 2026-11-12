//! The one copy-to-clipboard notice every copy key shares (Story 21.1 AC4).

use crate::adapters::tui::clipboard::ClipboardResult;
use crate::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource, notify};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::Conversation;

/// Generic Chat copy keeps its historical empty-string target resolution.
/// Tool-source copy deliberately never passes through this sentinel seam.
pub fn requested(
    state: &mut TuiState,
    conversation: &Conversation,
    content: String,
    copy: &dyn Fn(&str) -> ClipboardResult,
) {
    requested_in(state, conversation, None, content, copy);
}

/// As [`requested`], resolving a selected block of the still-open turn too.
pub fn requested_in(
    state: &mut TuiState,
    conversation: &Conversation,
    open_turn: Option<&crate::domain::models::Turn>,
    content: String,
    copy: &dyn Fn(&str) -> ClipboardResult,
) {
    let content = if content.is_empty() {
        state.resolve_copy_content_in(conversation, open_turn)
    } else {
        content
    };
    notify_copy(state, &content, copy);
}

/// Share the existing failure formatter without inventing a clipboard attempt.
pub(crate) fn notify_failure(state: &mut TuiState, reason: &str) {
    notifications::failure(
        state,
        NoticeSource::Clipboard,
        "Could not copy to the clipboard.",
        "Select the text in the terminal and copy it from there.",
        Some(reason.to_string()),
    );
}

/// Copies `text` through `copy` (production: `clipboard::copy_to_clipboard`)
/// and reports the outcome as a notice: Success for a copy, Info for empty
/// text, Error for a failed copy (the typed result decides, never the text).
pub(crate) fn notify_copy(
    state: &mut TuiState,
    text: &str,
    copy: &dyn Fn(&str) -> ClipboardResult,
) {
    notify_copy_with(state, text, copy, "Copied to clipboard");
}

/// [`notify_copy`] with the success wording of the thing copied (`Copied error to clipboard.`):
/// a copy that landed in the fallback file names the file in place of `clipboard`.
pub(crate) fn notify_copy_with(
    state: &mut TuiState,
    text: &str,
    copy: &dyn Fn(&str) -> ClipboardResult,
    success: &str,
) {
    let (severity, message) = if text.is_empty() {
        (NoticeSeverity::Info, "Nothing to copy".to_string())
    } else {
        match copy(text) {
            ClipboardResult::Osc52Success => (
                NoticeSeverity::Success,
                success.trim_end_matches('.').to_string(),
            ),
            ClipboardResult::FallbackSuccess(path) => (
                NoticeSeverity::Success,
                match success.trim_end_matches('.').strip_suffix(" to clipboard") {
                    Some(what) => format!("{what} to {}", path.display()),
                    None => format!("Copied to {}", path.display()),
                },
            ),
            ClipboardResult::Failed(error) => {
                notify_failure(state, &error);
                return;
            }
        }
    };
    notify(state, NoticeSource::Clipboard, severity, message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::StatusState;

    #[test]
    fn story21_5_generic_requested_preserves_explicit_bytes_and_chat_fallback() {
        let mut state = TuiState::new(80, 24);
        let mut conversation = Conversation::default();
        let mut turn = crate::domain::models::Turn::new("copy-model".into(), 0);
        turn.push_part(|id| crate::domain::models::TurnPart::Prose {
            id,
            text: "assistant fallback".into(),
        });
        conversation.turns.push(turn);
        conversation.rebuild_messages_mirror();
        state.selected_item_id = None;
        let copied = std::cell::RefCell::new(Vec::new());
        let copier = |text: &str| {
            copied.borrow_mut().push(text.to_owned());
            ClipboardResult::Osc52Success
        };
        requested(&mut state, &conversation, String::new(), &copier);
        requested(
            &mut state,
            &conversation,
            "\u{1b}[31mexplicit\r\n".into(),
            &copier,
        );
        assert_eq!(
            *copied.borrow(),
            ["assistant fallback", "\u{1b}[31mexplicit\r\n"]
        );
    }

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
                "Could not copy to the clipboard.\nSelect the text in the terminal and copy it from there.\nno tty",
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
