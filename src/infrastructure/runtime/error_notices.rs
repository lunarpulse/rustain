//! The runtime's failure notices: what happened, the next step, then the
//! source error text. Each function is one producer's copy so the event loops
//! stay a one-line call per failure.

use std::fmt::Display;

use crate::adapters::tui::handlers::transcript_target::{RewindFailure, RewindOutcome};
use crate::adapters::tui::notifications::{self, NoticeAction, NoticeDraft, NoticeSource};
use crate::adapters::tui::state::TuiState;
use crate::domain::errors::StorageError;

/// A file `@mention` could not be attached; `reason` already states the fact.
pub(crate) fn mention_unresolved(state: &mut TuiState, reason: &str) {
    notifications::failure(
        state,
        NoticeSource::Input,
        reason,
        "Check the path after the @, then mention the file again.",
        None,
    );
}

pub(crate) fn image_unsupported(state: &mut TuiState) {
    notifications::failure(
        state,
        NoticeSource::Image,
        "Could not attach the image.",
        "Use a PNG, JPEG, GIF or WebP image.",
        None,
    );
}

pub(crate) fn clipboard_unreadable(state: &mut TuiState, error: &dyn Display) {
    notifications::failure(
        state,
        NoticeSource::Clipboard,
        "Could not read the clipboard.",
        "Copy the content again, then paste again.",
        Some(error.to_string()),
    );
}

pub(crate) fn conversation_load_failed(state: &mut TuiState, error: &dyn Display) {
    notifications::failure(
        state,
        NoticeSource::Session,
        "Could not load the conversation.",
        "Pick it again from the history, or choose another conversation.",
        Some(error.to_string()),
    );
}

pub(crate) fn conversation_open_failed(state: &mut TuiState, error: &dyn Display) {
    notifications::failure(
        state,
        NoticeSource::Session,
        "Could not open the conversation.",
        "Pick it again from the search results, or search again.",
        Some(error.to_string()),
    );
}

pub(crate) fn fork_failed(state: &mut TuiState, error: &dyn Display) {
    notifications::failure(
        state,
        NoticeSource::Session,
        "Could not fork the conversation.",
        "Try forking again.",
        Some(error.to_string()),
    );
}

pub(crate) fn rewind_journal_failed(state: &mut TuiState, error: &dyn Display) {
    notifications::failure(
        state,
        NoticeSource::Session,
        "Could not start the rewind: the recovery journal was not created, so nothing changed.",
        "Check the workspace permissions and free disk space, then try the rewind again.",
        Some(error.to_string()),
    );
}

pub(crate) fn rewind_failed(state: &mut TuiState, error: &dyn Display) {
    notifications::failure(
        state,
        NoticeSource::Session,
        "Could not rewind the conversation.",
        "Try the rewind again.",
        Some(error.to_string()),
    );
}

/// A fork's outcome as the event loop receives it: the new conversation's id, or `None` after
/// raising the failure notice.
pub(crate) fn fork_result(
    state: &mut TuiState,
    result: Result<String, StorageError>,
) -> Option<String> {
    result
        .inspect_err(|error| {
            tracing::error!("Fork failed: {error}");
            fork_failed(state, error);
        })
        .ok()
}

/// A rewind's outcome as the event loop receives it: the outcome, or `None` after raising the
/// failure notice for the step that failed.
pub(crate) fn rewind_result(
    state: &mut TuiState,
    result: Result<RewindOutcome, RewindFailure>,
) -> Option<RewindOutcome> {
    match result {
        Ok(outcome) => Some(outcome),
        Err(RewindFailure::Begin(error)) => {
            tracing::error!("begin_rewind_txn failed — aborting rewind: {error}");
            rewind_journal_failed(state, &error);
            None
        }
        Err(RewindFailure::Truncate(error)) => {
            tracing::error!("revert_to_checkpoint failed: {error}");
            rewind_failed(state, &error);
            None
        }
    }
}

pub(crate) fn override_failed(
    state: &mut TuiState,
    label: &str,
    requested: &str,
    error: &dyn Display,
) {
    notifications::failure(
        state,
        NoticeSource::Adapter,
        format!("Could not switch the {label} adapter to {requested} for this session."),
        format!("Choose another adapter with /{label}."),
        Some(error.to_string()),
    );
}

/// The compaction failure toast; `r` in the log retries it.
pub(crate) fn compaction_failed(
    state: &mut TuiState,
    conversation_id: impl Into<String>,
    reason: &dyn Display,
) {
    notifications::notify_draft(
        state,
        NoticeDraft::failure(
            NoticeSource::Compaction,
            "Could not compact the conversation. The context is unchanged.",
            "Run /compact again.",
            Some(reason.to_string()),
        )
        .conversation(conversation_id)
        .action(NoticeAction::RetryCompaction),
    );
}

#[cfg(unix)]
pub(crate) fn attach_message_not_sent(state: &mut TuiState) {
    notifications::failure(
        state,
        NoticeSource::Attached,
        "Lost the connection to the daemon, so the message was not sent.",
        "Reattach with `rustain daemon attach`, then send it again.",
        None,
    );
}

#[cfg(unix)]
pub(crate) fn attach_stop_not_sent(state: &mut TuiState) {
    notifications::failure(
        state,
        NoticeSource::Attached,
        "Lost the connection to the daemon, so the stop request was not sent.",
        "Reattach with `rustain daemon attach`, then stop the turn again.",
        None,
    );
}

#[cfg(unix)]
pub(crate) fn daemon_reported_error(state: &mut TuiState, error: &dyn Display) {
    notifications::failure(
        state,
        NoticeSource::Daemon,
        "The daemon reported an error.",
        "Check the daemon with `rustain daemon status`, then try again.",
        Some(error.to_string()),
    );
}

/// The warning when a plan was still executing at the last exit.
pub(crate) fn plan_halted_at_exit(title: &str, task_number: impl Display) -> String {
    format!(
        "Plan '{title}' was mid-execution at exit (task {task_number} was running), so plan execution was halted. Open the task panel to resume it."
    )
}

/// The startup warning when the provider cannot be reached.
pub(crate) fn provider_offline(provider_name: impl Display) -> String {
    format!(
        "⚠ {provider_name}: offline. Configure a local model for offline use, or check your connection."
    )
}

/// The warning when a slash command points at a file that no longer exists.
pub(crate) fn missing_command_file(command: &str, path: impl Display) -> String {
    format!(
        "Command '/{command}' references missing file: {path}. Restore the file or change the path in the command."
    )
}

/// The warning when a task window is opened for a task that has since gone.
pub(crate) const TASK_UNAVAILABLE: &str =
    "That task is no longer available. Select another task from the list.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::notifications::NoticeSeverity;

    fn only_record(state: &TuiState) -> (NoticeSource, NoticeSeverity, Vec<String>) {
        assert_eq!(state.notifications.len(), 1);
        let record = state.notifications.records().next().unwrap();
        (
            record.source,
            record.severity,
            record.message.lines().map(str::to_owned).collect(),
        )
    }

    fn assert_shape(lines: &[String], expected: usize) {
        assert_eq!(lines.len(), expected, "{lines:?}");
        assert!(
            lines.iter().all(|line| !line.trim().is_empty()),
            "{lines:?}"
        );
    }

    #[test]
    fn three_line_failures_keep_source_severity_and_the_error_text() {
        let producers: [(fn(&mut TuiState, &dyn Display), NoticeSource, [&str; 2]); 6] = [
            (
                clipboard_unreadable,
                NoticeSource::Clipboard,
                [
                    "Could not read the clipboard.",
                    "Copy the content again, then paste again.",
                ],
            ),
            (
                conversation_load_failed,
                NoticeSource::Session,
                [
                    "Could not load the conversation.",
                    "Pick it again from the history, or choose another conversation.",
                ],
            ),
            (
                conversation_open_failed,
                NoticeSource::Session,
                [
                    "Could not open the conversation.",
                    "Pick it again from the search results, or search again.",
                ],
            ),
            (
                fork_failed,
                NoticeSource::Session,
                ["Could not fork the conversation.", "Try forking again."],
            ),
            (
                rewind_journal_failed,
                NoticeSource::Session,
                [
                    "Could not start the rewind: the recovery journal was not created, so nothing changed.",
                    "Check the workspace permissions and free disk space, then try the rewind again.",
                ],
            ),
            (
                rewind_failed,
                NoticeSource::Session,
                [
                    "Could not rewind the conversation.",
                    "Try the rewind again.",
                ],
            ),
        ];
        for (producer, source, [fact, next]) in producers {
            let mut state = TuiState::new(80, 24);
            producer(&mut state, &"disk \"full\"");
            let record = state.notifications.records().next().unwrap();
            assert_eq!(state.notifications.len(), 1);
            assert_eq!(
                (record.source, record.severity),
                (source, NoticeSeverity::Error)
            );
            assert_eq!(
                record.message,
                format!("{fact}\n{next}\ndisk \"full\""),
                "the fact, the next step, then the source text verbatim"
            );
        }
    }

    #[test]
    fn two_line_failures_name_the_fact_then_the_step() {
        let mut state = TuiState::new(80, 24);
        image_unsupported(&mut state);
        let record = state.notifications.records().next().unwrap();
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(
            (record.source, record.severity),
            (NoticeSource::Image, NoticeSeverity::Error)
        );
        assert_eq!(
            record.message,
            "Could not attach the image.\nUse a PNG, JPEG, GIF or WebP image."
        );
        let mut state = TuiState::new(80, 24);
        mention_unresolved(&mut state, "File not found: src/a.rs");
        let record = state.notifications.records().next().unwrap();
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(
            (record.source, record.severity),
            (NoticeSource::Input, NoticeSeverity::Error)
        );
        assert_eq!(
            record.message,
            "File not found: src/a.rs\n\
             Check the path after the @, then mention the file again."
        );
    }

    #[test]
    fn override_failure_names_the_port_the_adapter_and_the_error() {
        let mut state = TuiState::new(80, 24);
        override_failed(&mut state, "memory", "sqlite", &"no such adapter");
        let (source, severity, lines) = only_record(&state);
        assert_eq!(
            (source, severity),
            (NoticeSource::Adapter, NoticeSeverity::Error)
        );
        assert_shape(&lines, 3);
        assert_eq!(
            lines[0],
            "Could not switch the memory adapter to sqlite for this session."
        );
        assert_eq!(lines[1], "Choose another adapter with /memory.");
        assert_eq!(lines[2], "no such adapter");
    }

    #[test]
    fn compaction_failure_keeps_its_conversation_and_retry_action() {
        let mut state = TuiState::new(80, 24);
        compaction_failed(&mut state, "conv-1", &"LLM timeout");
        let record = state.notifications.records().next().unwrap();
        assert_eq!(record.source, NoticeSource::Compaction);
        assert_eq!(record.severity, NoticeSeverity::Error);
        assert_eq!(record.conversation_id.as_deref(), Some("conv-1"));
        assert_eq!(record.action, Some(NoticeAction::RetryCompaction));
        assert_eq!(
            record.message,
            "Could not compact the conversation. The context is unchanged.\n\
             Run /compact again.\nLLM timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn attached_failures_use_commands_the_attached_host_has() {
        let mut state = TuiState::new(80, 24);
        attach_message_not_sent(&mut state);
        let record = state.notifications.records().next().unwrap();
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(
            (record.source, record.severity),
            (NoticeSource::Attached, NoticeSeverity::Error)
        );
        assert_eq!(
            record.message,
            "Lost the connection to the daemon, so the message was not sent.\n\
             Reattach with `rustain daemon attach`, then send it again."
        );

        let mut state = TuiState::new(80, 24);
        attach_stop_not_sent(&mut state);
        let record = state.notifications.records().next().unwrap();
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(
            (record.source, record.severity),
            (NoticeSource::Attached, NoticeSeverity::Error)
        );
        assert_eq!(
            record.message,
            "Lost the connection to the daemon, so the stop request was not sent.\n\
             Reattach with `rustain daemon attach`, then stop the turn again."
        );

        let mut state = TuiState::new(80, 24);
        daemon_reported_error(&mut state, &"daemon error: boom");
        let record = state.notifications.records().next().unwrap();
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(
            (record.source, record.severity),
            (NoticeSource::Daemon, NoticeSeverity::Error)
        );
        assert_eq!(
            record.message,
            "The daemon reported an error.\n\
             Check the daemon with `rustain daemon status`, then try again.\n\
             daemon error: boom"
        );
    }

    #[test]
    fn the_event_loop_warnings_state_what_happened_and_one_next_step() {
        assert_eq!(
            plan_halted_at_exit("Ship it", 3),
            "Plan 'Ship it' was mid-execution at exit (task 3 was running), so plan execution was halted. Open the task panel to resume it."
        );
        assert_eq!(
            provider_offline("anthropic"),
            "⚠ anthropic: offline. Configure a local model for offline use, or check your connection."
        );
        assert_eq!(
            missing_command_file("review", "docs/review.md"),
            "Command '/review' references missing file: docs/review.md. Restore the file or change the path in the command."
        );
        assert_eq!(
            TASK_UNAVAILABLE,
            "That task is no longer available. Select another task from the list."
        );
    }

    fn conversation_with_one_message() -> crate::domain::models::Conversation {
        use crate::domain::models::{ChatMessage, ContentBlockType, MessageRole};
        crate::domain::models::Conversation {
            id: "conv-1".into(),
            messages: vec![ChatMessage {
                id: "m1".into(),
                role: MessageRole::User,
                content: "hello".into(),
                content_blocks: vec![ContentBlockType::Text],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The state a failed fork or rewind must leave alone, and the one record it raises.
    fn assert_session_failure(
        state: &TuiState,
        idle: &TuiState,
        expected: [&str; 2],
        detail: &str,
    ) {
        assert_eq!(state.status, idle.status, "the status is untouched");
        let (source, severity, lines) = only_record(state);
        assert_eq!(
            (source, severity),
            (NoticeSource::Session, NoticeSeverity::Error)
        );
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], expected[0]);
        assert_eq!(lines[1], expected[1]);
        assert_eq!(lines[2], detail);
        assert!(!detail.trim().is_empty(), "the source error text is kept");
    }

    #[tokio::test]
    async fn story21_14_a_fork_that_storage_refuses_raises_the_session_failure() {
        use crate::adapters::noop::NoOpStorage;
        use crate::adapters::tui::handlers::transcript_target::handle_fork_confirm;
        let conversation = conversation_with_one_message();
        // The flush is accepted; this storage cannot fork (the port's default refusal).
        let result = handle_fork_confirm(&NoOpStorage, &mut None, false, &conversation, 0).await;
        let detail = result
            .as_ref()
            .expect_err("this storage does not support forking")
            .to_string();
        let mut state = TuiState::new(80, 24);
        let idle = TuiState::new(80, 24);
        assert_eq!(fork_result(&mut state, result), None);
        assert_session_failure(
            &state,
            &idle,
            ["Could not fork the conversation.", "Try forking again."],
            &detail,
        );
        assert_eq!(
            conversation.messages.len(),
            1,
            "the conversation is untouched"
        );
    }

    /// A storage that cannot take the flush: the fork must not be attempted.
    struct RefusesTheFlush(std::sync::atomic::AtomicBool);

    #[async_trait::async_trait]
    impl crate::domain::ports::StoragePort for RefusesTheFlush {
        async fn save_conversation(
            &self,
            _conv: &crate::domain::models::Conversation,
        ) -> Result<(), crate::domain::errors::StorageError> {
            Err(crate::domain::errors::StorageError::IoError(
                "disk is full".into(),
            ))
        }

        async fn load_conversation(
            &self,
            _id: &str,
        ) -> Result<Option<crate::domain::models::Conversation>, crate::domain::errors::StorageError>
        {
            Ok(None)
        }

        async fn list_conversations(
            &self,
        ) -> Result<
            Vec<crate::domain::models::ConversationSummary>,
            crate::domain::errors::StorageError,
        > {
            Ok(Vec::new())
        }

        async fn read_snapshot(
            &self,
            _conversation_id: &str,
            _checkpoint: crate::domain::models::checkpoint::CheckpointId,
            _path: &std::path::Path,
        ) -> Result<Option<Vec<u8>>, crate::domain::errors::StorageError> {
            Ok(None)
        }

        async fn fork_at_checkpoint(
            &self,
            _source_conversation_id: &str,
            _checkpoint: crate::domain::models::checkpoint::CheckpointId,
        ) -> Result<String, crate::domain::errors::StorageError> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("forked-anyway".to_owned())
        }
    }

    #[tokio::test]
    async fn a_flush_that_storage_refuses_aborts_the_fork_and_raises_the_session_failure() {
        use crate::adapters::tui::handlers::transcript_target::handle_fork_confirm;
        let storage = RefusesTheFlush(std::sync::atomic::AtomicBool::new(false));
        let conversation = conversation_with_one_message();
        let result = handle_fork_confirm(&storage, &mut None, false, &conversation, 0).await;
        let detail = result.as_ref().expect_err("the flush failed").to_string();
        assert!(detail.contains("disk is full"), "{detail}");
        assert!(
            !storage.0.load(std::sync::atomic::Ordering::SeqCst),
            "no fork is attempted after a failed flush"
        );
        let mut state = TuiState::new(80, 24);
        let idle = TuiState::new(80, 24);
        assert_eq!(fork_result(&mut state, result), None);
        assert_session_failure(
            &state,
            &idle,
            ["Could not fork the conversation.", "Try forking again."],
            &detail,
        );
    }

    #[tokio::test]
    async fn story21_14_a_fork_that_succeeds_raises_nothing() {
        let mut state = TuiState::new(80, 24);
        assert_eq!(
            fork_result(&mut state, Ok("conv-2".to_owned())),
            Some("conv-2".to_owned())
        );
        assert_eq!(state.notifications.len(), 0);
    }

    #[tokio::test]
    async fn story21_14_a_rewind_that_cannot_truncate_raises_the_rewind_failure() {
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::adapters::tui::handlers::transcript_target::handle_rewind_confirm;
        let dir = tempfile::tempdir().unwrap();
        let storage = FileSystemStorage::new(dir.path().to_path_buf());
        let conversation = conversation_with_one_message();
        let result = handle_rewind_confirm(&storage, &conversation, 0).await;
        let detail = match &result {
            Err(RewindFailure::Truncate(error)) => error.to_string(),
            other => panic!(
                "expected a truncate failure, got {:?}",
                other.as_ref().err()
            ),
        };
        let mut state = TuiState::new(80, 24);
        let idle = TuiState::new(80, 24);
        assert!(rewind_result(&mut state, result).is_none());
        assert_session_failure(
            &state,
            &idle,
            [
                "Could not rewind the conversation.",
                "Try the rewind again.",
            ],
            &detail,
        );
        assert_eq!(
            conversation.messages.len(),
            1,
            "the conversation is untouched"
        );
    }

    #[tokio::test]
    async fn story21_14_a_rewind_without_a_journal_says_nothing_changed() {
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::adapters::tui::handlers::transcript_target::handle_rewind_confirm;
        let dir = tempfile::tempdir().unwrap();
        let not_a_directory = dir.path().join("sessions");
        std::fs::write(
            &not_a_directory,
            "a file where the sessions folder should be",
        )
        .unwrap();
        let storage = FileSystemStorage::new(not_a_directory);
        let conversation = conversation_with_one_message();
        let result = handle_rewind_confirm(&storage, &conversation, 0).await;
        let detail = match &result {
            Err(RewindFailure::Begin(error)) => error.to_string(),
            other => panic!("expected a journal failure, got {:?}", other.as_ref().err()),
        };
        let mut state = TuiState::new(80, 24);
        let idle = TuiState::new(80, 24);
        assert!(rewind_result(&mut state, result).is_none());
        assert_session_failure(
            &state,
            &idle,
            [
                "Could not start the rewind: the recovery journal was not created, so nothing changed.",
                "Check the workspace permissions and free disk space, then try the rewind again.",
            ],
            &detail,
        );
    }
}
