//! `AppEvent::SystemNotice` consumer effects, extracted from `event_loop::run`
//! so the production caller and the K2 keystone drive the same code (Story 21.2
//! AC2).
//!
//! Only a turn-fatal notice (`NoticeLevel::is_turn_fatal`: Warning/Error) resets
//! streaming state and cancels the active turn. `Advisory` — the level of an
//! input-validation notice such as an unknown slash command — is rendered with
//! the same warning row but leaves the live stream, its cancellation token and
//! its tool blocks untouched.

use crate::adapters::tui::keymap::Context;
use crate::adapters::tui::keymap::render::{key_text, rail_key};
use crate::adapters::tui::keymap::table::LeaderCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::notifications::{
    NoticeAction, NoticeDraft, NoticeSeverity, NoticeSource, notify_draft,
};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::tab::TabManager;
use crate::domain::models::{
    Conversation, NoticeLevel, StatusState, StreamingPhase, StreamingState,
};
use crate::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle;

/// Apply a notice addressed to the ACTIVE conversation (or a global one).
pub(crate) fn apply_active_notice(
    state: &mut TuiState,
    streaming: &mut StreamingState,
    tabs: &mut TabManager,
    conversation: &mut Conversation,
    lifecycle: &mut TuiTurnLifecycle,
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

    // Story 21.1 AC4: every level is one store notice attributed to its
    // conversation. No block, no flash, no queue, and never a focus move: the
    // notice appears as a toast and stays reviewable in S-LOG. An Error states
    // what failed and the way forward, and carries the retry handle of the turn
    // that failed.
    let retry = NoticeAction::RetryTurn {
        messages: conversation.messages.len(),
    };
    let draft = if matches!(level, NoticeLevel::Error) {
        state.status = StatusState::Idle;
        failure_draft(&msg, true).action(retry)
    } else {
        NoticeDraft::new(NoticeSource::System, NoticeSeverity::from_level(level), msg)
    };
    notify_draft(state, draft.conversation(conversation.id.clone()));
    state.needs_redraw = true;
}

/// The Error draft of a failed turn: the provider's first line is what
/// happened (further lines stay as the detail), and the next step names the
/// keys that retry it, generated from the registry. An offline failure is
/// rewritten into plain words. `retry` says whether the record carries a
/// retry handle.
fn failure_draft(msg: &str, retry: bool) -> NoticeDraft {
    if msg.starts_with("Offline:") {
        return NoticeDraft::failure(
            NoticeSource::System,
            "No provider available.",
            "Configure a local model for offline use, or check your connection.",
            None,
        );
    }
    let mut lines = msg.lines().map(str::trim).filter(|line| !line.is_empty());
    let what = lines.next().unwrap_or("The turn failed.");
    let rest = lines.collect::<Vec<_>>().join("\n");
    let log = key_text(LeaderCmd::Notifications);
    let next = if retry {
        let key = rail_key(Context::NotificationLog, OverlayCmd::NotifRetry);
        format!("Retry: open the log with {log} and press {key}.")
    } else {
        format!("Send the message again, or open the log with {log} for details.")
    };
    NoticeDraft::failure(
        NoticeSource::System,
        what,
        next,
        (!rest.is_empty()).then_some(rest),
    )
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
    // Story 21.1 AC4: a notice addressed to a background conversation is an
    // attributed store record (no per-tab stash): it survives the switch in
    // S-LOG with the conversation named, and it paints as a toast now. Its
    // Error carries no retry handle, so the next step says to send again.
    let draft = if matches!(level, NoticeLevel::Error) {
        failure_draft(&msg, false)
    } else {
        NoticeDraft::new(NoticeSource::System, NoticeSeverity::from_level(level), msg)
    };
    notify_draft(state, draft.conversation(conversation_id));
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
        // The advisory is a visible warning toast.
        assert!(f.state.needs_redraw);
        assert_eq!(f.state.notifications.visible().count(), 1);
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

    fn records(f: &Fixture) -> Vec<(NoticeSeverity, String, Option<String>, Option<NoticeAction>)> {
        f.state
            .notifications
            .records()
            .map(|r| {
                (
                    r.severity,
                    r.message.clone(),
                    r.conversation_id.clone(),
                    r.action,
                )
            })
            .collect()
    }

    /// K5 — the default view no longer hides any notice, and no level
    /// moves focus (D2, D8): the consumer is the production front door.
    #[tokio::test]
    async fn story21_1_default_shows_every_level_and_nothing_moves_focus() {
        use crate::domain::models::FocusState;
        for (level, severity) in [
            (NoticeLevel::Info, NoticeSeverity::Info),
            (NoticeLevel::Advisory, NoticeSeverity::Warning),
            (NoticeLevel::Warning, NoticeSeverity::Warning),
            (NoticeLevel::Error, NoticeSeverity::Error),
        ] {
            let mut f = live_stream();
            f.state.focus = FocusState::Input;
            apply(&mut f, level, "visible in focus");
            assert_eq!(
                f.state.focus,
                FocusState::Input,
                "{level:?} never takes focus"
            );
            let visible: Vec<_> = f.state.notifications.visible().collect();
            assert_eq!(visible.len(), 1, "{level:?}");
            assert_eq!(visible[0].severity, severity);
            assert_eq!(
                visible[0].conversation_id.as_deref(),
                Some(f.conversation.id.as_str())
            );
            assert!(
                f.state.feedback_blocks.is_empty(),
                "{level:?}: no transcript block"
            );
        }
    }

    #[tokio::test]
    async fn story21_1_error_carries_the_retry_handle_and_idles_the_status() {
        let mut f = live_stream();
        f.state.status = StatusState::Streaming;
        apply(&mut f, NoticeLevel::Error, "provider failed");
        assert_eq!(f.state.status, StatusState::Idle);
        let id = f.conversation.id.clone();
        assert_eq!(
            records(&f),
            vec![(
                NoticeSeverity::Error,
                "provider failed\n\
                 Retry: open the log with Ctrl+X, N and press r."
                    .to_owned(),
                Some(id),
                Some(NoticeAction::RetryTurn {
                    messages: f.conversation.messages.len()
                })
            )]
        );
        // Warning/Advisory/Info never carry an action.
        let mut g = live_stream();
        apply(&mut g, NoticeLevel::Warning, "w");
        assert_eq!(g.state.notifications.records().next().unwrap().action, None);
    }

    /// DF-266: repeated identical notices coalesce instead of flooding.
    #[tokio::test]
    async fn story21_1_repeated_identical_notices_coalesce() {
        let mut f = live_stream();
        for _ in 0..5 {
            apply(
                &mut f,
                NoticeLevel::Advisory,
                "r is reserved while a task runs",
            );
        }
        assert_eq!(f.state.notifications.len(), 1);
        assert_eq!(f.state.notifications.records().next().unwrap().count, 5);
    }

    /// A background-tab notice is an attributed record, not a per-tab stash.
    #[tokio::test]
    async fn story21_1_background_notice_is_an_attributed_record() {
        let mut f = live_stream();
        let id = f.conversation.id.clone();
        apply_background_notice(
            &mut f.state,
            &mut f.tabs,
            &id,
            NoticeLevel::Error,
            "late failure".into(),
        );
        assert_eq!(
            records(&f),
            vec![(
                NoticeSeverity::Error,
                "late failure\n\
                 Send the message again, or open the log with Ctrl+X, N for details."
                    .to_owned(),
                Some(id),
                None
            )],
            "attributed, and the background arm carries no retry handle"
        );
        assert!(
            f.tabs.active_tab().feedback_blocks.is_empty(),
            "no bgfb stash"
        );
    }

    /// 21-14 AC17 (K10, M12): a turn failure keeps the provider text as the fact, names the
    /// generated retry path as the next step, and keeps the retry handle.
    #[tokio::test]
    async fn story21_14_turn_failure_names_the_retry_path() {
        let mut f = live_stream();
        apply(&mut f, NoticeLevel::Error, "provider failed");
        let record = f.state.notifications.records().next().expect("one record");
        assert_eq!(
            record.message.lines().collect::<Vec<_>>(),
            vec![
                "provider failed",
                "Retry: open the log with Ctrl+X, N and press r."
            ]
        );
        assert!(matches!(
            record.action,
            Some(NoticeAction::RetryTurn { .. })
        ));
        // The keys come from the registry rows, not from literals.
        assert!(record.message.contains(&key_text(LeaderCmd::Notifications)));
        assert!(
            record
                .message
                .contains(&rail_key(Context::NotificationLog, OverlayCmd::NotifRetry))
        );
    }

    #[tokio::test]
    async fn story21_14_provider_text_beyond_the_first_line_is_kept_as_the_detail() {
        let mut f = live_stream();
        apply(
            &mut f,
            NoticeLevel::Error,
            "Provider returned 529\n  overloaded_error: try later\n",
        );
        let record = f.state.notifications.records().next().expect("one record");
        assert_eq!(
            record.message,
            "Provider returned 529\n\
             Retry: open the log with Ctrl+X, N and press r.\n\
             overloaded_error: try later"
        );
    }

    #[tokio::test]
    async fn story21_14_an_offline_failure_is_rewritten_into_plain_words_with_its_retry_handle() {
        let mut f = live_stream();
        apply(&mut f, NoticeLevel::Error, "Offline: no route to host");
        let record = f.state.notifications.records().next().expect("one record");
        assert_eq!(
            record.message,
            "No provider available.\n\
             Configure a local model for offline use, or check your connection."
        );
        assert!(matches!(
            record.action,
            Some(NoticeAction::RetryTurn { .. })
        ));
    }

    /// 14-C-01: an oversized provider error (one huge line, ASCII or multibyte, with or without
    /// a long continuation) can never push the mandatory next step out of the stored record.
    #[tokio::test]
    async fn story21_14_an_oversized_provider_error_keeps_its_next_step_line() {
        use crate::adapters::tui::notifications::MAX_MESSAGE_BYTES;
        let retry = "Retry: open the log with Ctrl+X, N and press r.";
        let huge_ascii = "x".repeat(2 * MAX_MESSAGE_BYTES);
        let huge_cjk = "界".repeat(MAX_MESSAGE_BYTES);
        let with_detail = format!("{}\n{}", "e".repeat(40_000), "d".repeat(40_000));
        for text in [huge_ascii, huge_cjk, with_detail] {
            let mut f = live_stream();
            apply(&mut f, NoticeLevel::Error, &text);
            let record = f.state.notifications.records().next().expect("one record");
            assert!(
                record.message.len() <= MAX_MESSAGE_BYTES,
                "{}",
                record.message.len()
            );
            let lines: Vec<&str> = record.message.lines().collect();
            assert!(lines.len() >= 2, "the next step survives the cap");
            assert_eq!(lines[1], retry, "line two is the retry path");
            assert!(
                lines[0].ends_with('…'),
                "the fact is clipped with an ellipsis"
            );
            assert!(matches!(
                record.action,
                Some(NoticeAction::RetryTurn { .. })
            ));
            // The background arm keeps its own next step the same way.
            let id = f.conversation.id.clone();
            apply_background_notice(&mut f.state, &mut f.tabs, &id, NoticeLevel::Error, text);
            let last = f.state.notifications.records().last().expect("a record");
            assert_eq!(
                last.message.lines().nth(1),
                Some("Send the message again, or open the log with Ctrl+X, N for details.")
            );
        }
    }

    /// R2-01: a provider diagnostic that fits the record cap with its next step is stored
    /// INTACT (no arbitrary smaller quota), ASCII and multibyte.
    #[tokio::test]
    async fn story21_14_a_provider_error_below_the_cap_is_kept_whole() {
        let retry = "Retry: open the log with Ctrl+X, N and press r.";
        for text in ["p".repeat(10 * 1024), "界".repeat(3_000), "é".repeat(5_000)] {
            let mut f = live_stream();
            apply(&mut f, NoticeLevel::Error, &text);
            let record = f.state.notifications.records().next().expect("one record");
            let lines: Vec<&str> = record.message.lines().collect();
            assert_eq!(
                lines,
                vec![text.as_str(), retry],
                "the whole provider text survives"
            );
        }
    }

    /// AC1 through the consumer: the deadline comes from the injected clock.
    #[tokio::test]
    async fn story21_1_consumer_notices_expire_on_the_injected_clock() {
        use crate::adapters::tui::notifications::{NotificationStore, begin_draw};
        use crate::domain::clock::MockClock;
        use crate::domain::models::NotificationsConfig;
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let clock = Arc::new(MockClock::new(Instant::now()));
        let mut f = live_stream();
        f.state.notifications =
            NotificationStore::new(clock.clone(), NotificationsConfig::default().clamped());
        apply(&mut f, NoticeLevel::Info, "info");
        apply(&mut f, NoticeLevel::Error, "error");
        clock.advance(Duration::from_secs(4) - Duration::from_millis(1));
        begin_draw(&mut f.state);
        assert_eq!(f.state.notifications.visible().count(), 2);
        clock.advance(Duration::from_millis(1));
        begin_draw(&mut f.state);
        let left: Vec<_> = f
            .state
            .notifications
            .visible()
            .map(|r| r.message.clone())
            .collect();
        assert_eq!(
            left,
            ["error\nRetry: open the log with Ctrl+X, N and press r."],
            "info expired at 4 s; the error lives 60 s"
        );
        clock.advance(Duration::from_secs(56));
        begin_draw(&mut f.state);
        assert_eq!(f.state.notifications.visible().count(), 0);
    }

    /// Offer-only catalogue for the real turn driver below.
    struct Catalogue;

    #[async_trait::async_trait]
    impl crate::domain::ports::ToolSetPort for Catalogue {
        fn available_tools(&self) -> Vec<crate::domain::models::ToolDefinition> {
            ["Read", "Bash", "task"]
                .iter()
                .map(|name| crate::domain::models::ToolDefinition {
                    name: (*name).to_string(),
                    description: format!("{name} tool"),
                    input_schema: serde_json::json!({"type": "object", "properties": {}}),
                    parallel_safe: true,
                })
                .collect()
        }

        async fn execute(
            &self,
            tool_name: &str,
            _input: serde_json::Value,
            _cancel: CancellationToken,
        ) -> Result<crate::domain::models::ToolResult, crate::domain::errors::ToolError> {
            Err(crate::domain::errors::ToolError::NotFound(tool_name.into()))
        }
    }

    /// K5 / FR42-a through the REAL producer: `LocalTurnDriver::submit`
    /// (`turn_driver.rs`, the unhonourable-restriction disclosure) emits the
    /// Advisory; the production consumer (`event_loop.rs` SystemNotice arm →
    /// `apply_active_notice`) turns it into a warning toast that is painted in
    /// the default view and never ends the live turn (D2).
    #[tokio::test(flavor = "multi_thread")]
    async fn story21_1_fr42a_advisory_from_the_real_turn_driver_is_visible_in_focus() {
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::adapters::noop::{
            NoOpApprovalPersistence, NoOpContext, NoOpPersona, NoOpProvider, NoOpSecurity,
            NoOpStorage, NoOpUsageLedger,
        };
        use crate::adapters::skill_activation::SkillActivator;
        use crate::adapters::tui::notification_log::LogFacts;
        use crate::adapters::tui::widgets::toast_layer;
        use crate::domain::events::AppEvent;
        use crate::domain::models::{ActiveAgent, SessionManager, SessionState};
        use crate::domain::ports::{ContextAssemblerPort, ContextPort, ToolSetPort};
        use crate::domain::services::approval_runtime::ApprovalRuntime;
        use crate::domain::services::tool_scheduler::ToolScheduler;
        use crate::infrastructure::runtime::turn_driver::{
            LocalTurnDriver, TurnViewState, UserSubmission,
        };
        use arc_swap::ArcSwap;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::sync::Arc;

        let workspace = tempfile::tempdir().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let security = Arc::new(NoOpSecurity);
        let tools: Arc<dyn ToolSetPort> = Arc::new(Catalogue);
        let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval, 16);
        let driver = LocalTurnDriver::new(
            Arc::new(NoOpProvider),
            Arc::new(ArcSwap::from_pointee(
                crate::domain::models::AppConfig::default(),
            )),
            event_tx,
            security,
            tools,
            scheduler,
            Arc::new(SkillActivator::new()),
            Arc::new(NoOpPersona),
            Arc::new(ArcSwap::from_pointee(
                Arc::new(NoOpContext) as Arc<dyn ContextPort>
            )),
            Arc::new(ArcSwap::from_pointee(
                None as Option<Arc<dyn ContextAssemblerPort>>,
            )),
            workspace.path().to_path_buf(),
            Arc::new(FileSystemStorage::new(workspace.path().join("sessions"))),
            Arc::new(NoOpStorage),
            Arc::new(crate::domain::services::plan_mode_injector::DefaultPlanInjector::new()),
            Arc::new(NoOpUsageLedger),
            crate::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        );
        let mut turn_conversation = Conversation::default();
        let mut turn_streaming = StreamingState::default();
        let mut turn_state = TuiState::new(120, 24);
        let mut turn_lifecycle = TuiTurnLifecycle::default();
        let mut session_manager = SessionManager::new(SessionState::Empty);
        let agent = ActiveAgent {
            name: "no-kubectl".to_string(),
            file: workspace.path().join("no-kubectl.md"),
            body: String::new(),
            allowed_tools: None,
            exclude_tools: Some(vec!["Bash(kubectl:*)".to_string()]),
            model: None,
        };
        driver
            .submit(
                UserSubmission {
                    text: "deploy billing to production".into(),
                    images: vec![],
                    synthetic: false,
                    activation_set: None,
                    agent_snapshot: Some(agent),
                    turn_cancel: CancellationToken::new(),
                },
                TurnViewState {
                    conversation: &mut turn_conversation,
                    streaming: &mut turn_streaming,
                    state: &mut turn_state,
                    lifecycle: &mut turn_lifecycle,
                    session_manager: &mut session_manager,
                },
            )
            .await;
        if let Some(handle) = turn_lifecycle.take_task(&turn_conversation.id) {
            tokio::time::timeout(std::time::Duration::from_secs(30), handle)
                .await
                .expect("the turn finishes")
                .expect("the turn task does not panic");
        }
        let mut advisories = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let Some((AppEvent::SystemNotice { level, message, .. }, _)) =
                turn_lifecycle.admit(event)
                && level == NoticeLevel::Advisory
            {
                advisories.push((level, message));
            }
        }
        assert_eq!(advisories.len(), 1, "one FR42-a disclosure: {advisories:?}");

        // The consumer side: a live stream.
        let mut f = live_stream();
        for (level, message) in advisories {
            apply_active_notice(
                &mut f.state,
                &mut f.streaming,
                &mut f.tabs,
                &mut f.conversation,
                &mut f.lifecycle,
                level,
                message,
            );
        }
        assert!(f.streaming.is_streaming, "an Advisory never ends the turn");
        assert!(!f.token.is_cancelled());
        let visible: Vec<_> = f.state.notifications.visible().collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].severity, NoticeSeverity::Warning);
        assert!(visible[0].message.contains("Bash(kubectl:*)"));

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let theme = f.state.theme.clone();
        let store = &f.state.notifications;
        terminal
            .draw(|frame| {
                let pane = ratatui::layout::Rect::new(0, 0, 100, 20);
                toast_layer::render(frame, pane, store, &theme, &LogFacts::default());
            })
            .unwrap();
        let painted: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // The wide toast is a half-pane box: the severity word and the head of
        // the first line are painted (the rest is ellipsized).
        assert!(
            painted.contains("Warning") && painted.contains("Active tool restriction"),
            "the Advisory is painted by default: {}",
            painted.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }

    /// AC14 — the owner-screenshot storm, replayed through the production
    /// consumer on a `MockClock`: a startup recovery prompt, two discovery
    /// warnings, a model switch, a login result, an unknown-slash Advisory, a
    /// compaction failure and five identical errors. 61 s after the last event
    /// the notice area is empty, every fact is in the log with the right count,
    /// `Log !N` counts the warning/error occurrences, the recovery decision is
    /// still pending in the pane and no `[y/n]` / `Ctrl+K` text was ever painted.
    #[tokio::test]
    async fn story21_1_owner_screenshot_storm_leaves_an_empty_notice_area_and_every_fact_in_the_log()
     {
        use crate::adapters::tui::decision_queue::{self, Arrival, DecisionDraft, DecisionKind};
        use crate::adapters::tui::keymap::Host;
        use crate::adapters::tui::layout::compute_layout;
        use crate::adapters::tui::notification_log::LogFacts;
        use crate::adapters::tui::notifications::{self, NotificationStore, begin_draw};
        use crate::adapters::tui::presentation::{self, Shell, Surface, is_open};
        use crate::adapters::tui::widgets::toast_layer;
        use crate::domain::clock::MockClock;
        use crate::domain::models::NotificationsConfig;
        use ratatui::layout::Rect;
        use ratatui::widgets::Paragraph;
        use ratatui::{Terminal, backend::TestBackend};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let clock = Arc::new(MockClock::new(Instant::now()));
        let mut f = live_stream();
        f.state.notifications =
            NotificationStore::new(clock.clone(), NotificationsConfig::default().clamped());
        let layout = compute_layout(Rect::new(0, 0, 80, 24), &f.state.theme, "", 1, false).unwrap();
        f.state
            .notifications
            .set_capacity(toast_layer::capacity_for(layout.chat_pane.height));
        let facts = LogFacts {
            active_conversation_id: Some(f.conversation.id.clone()),
            decisions_supported: true,
            ..LogFacts::default()
        };

        // Full pane draws use the host's shared compositor; toast-only and bare
        // stage controls still isolate expiry without covering the notice area.
        let paint = |f: &mut Fixture, toasts: bool, pane: bool| -> String {
            begin_draw(&mut f.state);
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let theme = f.state.theme.clone();
            let mut pane_painted = false;
            terminal
                .draw(|frame| {
                    let chat = layout.chat_pane;
                    let rows: Vec<String> = (0..chat.height)
                        .map(|row| format!("stage row {row:02} ").repeat(8))
                        .collect();
                    frame.render_widget(Paragraph::new(rows.join("\n")), chat);
                    if pane {
                        let shell = Shell::new(frame.area(), &layout);
                        pane_painted = presentation::compose(
                            frame,
                            &f.state,
                            &shell,
                            Host::Local,
                            &facts,
                            &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                            |_| {},
                        );
                    } else if toasts {
                        toast_layer::render(frame, chat, &f.state.notifications, &theme, &facts);
                    }
                })
                .unwrap();
            if pane_painted {
                decision_queue::note_painted(&mut f.state);
            }
            let buffer = terminal.backend().buffer().clone();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol().to_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut every_screen = String::new();

        // The storm. A startup recovery prompt first (0→1, empty composer: it takes focus).
        decision_queue::raise(
            &mut f.state,
            DecisionDraft::new(DecisionKind::Recovery, "Recovered chat · 1234 tokens"),
            Arrival::Automatic,
        );
        let tick = Duration::from_millis(200);
        clock.advance(tick);
        apply(
            &mut f,
            NoticeLevel::Warning,
            "Discovery: 2 skills skipped (a)",
        );
        clock.advance(tick);
        apply(
            &mut f,
            NoticeLevel::Warning,
            "Discovery: 1 agent skipped (b)",
        );
        clock.advance(tick);
        notifications::success(
            &mut f.state,
            NoticeSource::Model,
            "Switched to claude-opus-4",
        );
        clock.advance(tick);
        notifications::success(&mut f.state, NoticeSource::Login, "Signed in to Anthropic");
        clock.advance(tick);
        apply(
            &mut f,
            NoticeLevel::Advisory,
            "Unknown command: /nope — not a built-in or a custom command.",
        );
        clock.advance(tick);
        notifications::notify_draft(
            &mut f.state,
            NoticeDraft::new(
                NoticeSource::Compaction,
                NoticeSeverity::Error,
                "Compaction failed: provider error",
            )
            .action(NoticeAction::RetryCompaction),
        );
        for _ in 0..5 {
            clock.advance(tick);
            apply(&mut f, NoticeLevel::Error, "Provider failed: 503");
        }

        // Positive control: right after the storm the notice area is NOT empty.
        let during = paint(&mut f, true, true);
        every_screen.push_str(&during);
        let stage_only = paint(&mut f, false, false);
        assert_ne!(during, stage_only, "the storm paints toasts and the pane");
        assert!(f.state.notifications.visible().count() > 0);

        // 61 s after the last event: no toast is left, nothing else changed.
        clock.advance(Duration::from_secs(61));
        let toasts_only = paint(&mut f, true, false);
        assert_eq!(
            toasts_only,
            paint(&mut f, false, false),
            "an empty notice area leaves every stage cell untouched"
        );
        assert_eq!(f.state.notifications.visible().count(), 0);
        let last = paint(&mut f, true, true);
        every_screen.push_str(&last);

        // Every fact is in the log; repeated errors are one record counted ×5.
        let facts_in_log: Vec<(String, u32)> = f
            .state
            .notifications
            .records()
            .map(|r| (r.message.clone(), r.count))
            .collect();
        for (message, count) in [
            ("Discovery: 2 skills skipped (a)", 1),
            ("Discovery: 1 agent skipped (b)", 1),
            ("Switched to claude-opus-4", 1),
            ("Signed in to Anthropic", 1),
            (
                "Unknown command: /nope — not a built-in or a custom command.",
                1,
            ),
            ("Compaction failed: provider error", 1),
            (
                "Provider failed: 503\nRetry: open the log with Ctrl+X, N and press r.",
                5,
            ),
        ] {
            assert!(
                facts_in_log.contains(&(message.to_owned(), count)),
                "{message} ×{count} missing from {facts_in_log:?}"
            );
        }
        // `Log !N`: warning/error occurrences nobody presented (2 + 1 advisory + 1 + 5).
        assert_eq!(f.state.notifications.unseen(), 9);

        // The recovery decision is still pending, focused, and painted with its own rail.
        assert_eq!(f.state.decisions.len(), 1);
        assert!(is_open(&f.state, Surface::Decision));
        assert!(presentation::consistent(&f.state));
        assert_eq!(f.state.modal_stack.len(), 1);
        assert_eq!(
            f.state.modal_stack.top().unwrap().surface,
            Surface::Decision
        );
        assert!(
            last.contains("[Enter/Esc] Continue") && last.contains("[n] New session"),
            "{last}"
        );
        assert!(
            !every_screen.contains("[y/n]") && !every_screen.contains("Ctrl+K"),
            "retired prompt text was painted"
        );
    }
}
