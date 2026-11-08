//! K12 card/focus controls: real approval outcomes and DF-182 trust ownership.
use super::k3_compose::open_quit;
use super::keymap_support::press;
use rustain::adapters::noop::NoOpApprovalPersistence;
use rustain::adapters::tui::{
    app::InputAction,
    decision_queue::{self, DecisionKind, DecisionSubject, SkillOrigin},
    keymap::Key,
    notification_log::LogFacts,
    notifications::NotificationStore,
    presentation::{self, Frame, Surface, reducer},
    state::TuiState,
};
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    ApprovalOutcome, FocusState, NotificationsConfig, OverlayType, PermissionResolution, RequestId,
    SkillTrustResponse, ToolRisk,
    palette::{PaletteAction, PaletteEntry, PaletteScope},
    tool_call::ApprovalSource,
    visual::ConfirmationType,
};
use rustain::domain::services::approval_runtime::{
    ApprovalRuntime, ApprovalRuntimeEvent, ResolvedApproval, ReviewScope,
};
use rustain::infrastructure::runtime::approval_bridge;
use std::sync::Arc;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::{broadcast, oneshot};

struct Approvals {
    runtime: Arc<ApprovalRuntime>,
    events: broadcast::Receiver<ApprovalRuntimeEvent>,
}

impl Approvals {
    fn new() -> Self {
        let runtime = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
        let events = runtime.subscribe();
        Self { runtime, events }
    }

    async fn request(
        &mut self,
        state: &mut TuiState,
    ) -> (RequestId, oneshot::Receiver<ResolvedApproval>) {
        // The covered-card controls exercise RU-6's recent-typing guard;
        // an idle non-text frame may otherwise be interrupted.
        if !state.modal_stack.is_empty() {
            state.last_printable_at = Some(state.notifications.now());
        }
        let (id, response) = self
            .runtime
            .request(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "legacy-card-control".into(),
                },
                "Read".into(),
                serde_json::json!({"path": "fixture.txt"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let id = id.expect("foreground approval must enter the real pending table");
        let event = self.events.try_recv().expect("synchronous Requested event");
        assert!(
            matches!(&event, ApprovalRuntimeEvent::Requested { id: actual, .. } if actual == &id)
        );
        let review = self
            .runtime
            .pending_review(&id, ReviewScope::AllTools)
            .await;
        assert!(review.is_some());
        approval_bridge::apply(state, event, review);
        (id, response)
    }

    fn apply_resolution(&mut self, state: &mut TuiState, id: &RequestId) {
        let event = self.events.try_recv().expect("synchronous Resolved event");
        assert!(
            matches!(&event, ApprovalRuntimeEvent::Resolved { id: actual, .. } if actual == id)
        );
        approval_bridge::apply(state, event, None);
    }
}

fn confirmation(kind: ConfirmationType) -> FocusState {
    FocusState::Overlay(OverlayType::Confirmation(kind))
}

fn frames(state: &TuiState) -> Vec<Frame> {
    state.modal_stack.iter().cloned().collect()
}

fn assert_queued(state: &TuiState) {
    assert_eq!(state.decisions.len(), 1);
    assert!(!presentation::is_open(state, Surface::Decision));
}

/// M11: an actual queued request must not interpret a palette query's `a`.
#[tokio::test]
async fn card_under_frames_never_takes_the_keys() {
    let mut state = TuiState::new(80, 24);
    state.input_buffer = "retained composer".into();
    state.cursor_position = 4;
    assert_eq!(press(&mut state, Key::Ctrl('p')), InputAction::Consumed);
    for c in "hel".chars() {
        assert_eq!(press(&mut state, Key::Char(c)), InputAction::Consumed);
    }
    assert_eq!(state.command_palette.filter_text.as_str(), "hel");
    let palette = state.modal_stack.top().unwrap().clone();
    assert_eq!(palette.surface, Surface::CommandPalette);
    let base = state.focus.clone();
    let mut approvals = Approvals::new();
    let (id, mut response) = approvals.request(&mut state).await;
    assert_queued(&state);
    let input = approvals
        .runtime
        .pending_review(&id, ReviewScope::AllTools)
        .await
        .unwrap()
        .input;
    assert_eq!(state.focus, base);
    assert_eq!(press(&mut state, Key::Char('a')), InputAction::Consumed);
    assert_eq!(state.command_palette.filter_text.as_str(), "hela");
    assert_eq!(state.modal_stack.top(), Some(&palette));
    assert_eq!(state.focus, base);
    assert_queued(&state);
    assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
    assert!(
        approvals
            .runtime
            .pending_review(&id, ReviewScope::AllTools)
            .await
            .is_some()
    );
    assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
    assert!(state.modal_stack.is_empty());
    assert_queued(&state);
    assert!(rustain::adapters::tui::decision_queue::holds_request(
        &state, &id
    ));
    let item = state.decisions.items().next().unwrap();
    let rustain::adapters::tui::decision_queue::DecisionSubject::Permission(payload) =
        &item.subject
    else {
        panic!("expected permission payload");
    };
    let rustain::adapters::tui::decision_queue::PermissionInput::Ready(held) = &payload.input
    else {
        panic!("expected exact input");
    };
    assert!(Arc::ptr_eq(held, &input));
    assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(state.focus, FocusState::Input);
    assert_eq!(state.input_buffer, "retained composer");
    assert_eq!(state.cursor_position, 4);
    assert!(presentation::consistent(&state));
}

/// Empty-base arrivals use a decision frame, without borrowing base focus.
#[tokio::test]
async fn empty_stack_permission_arrival_opens_a_frame_without_borrowing_focus() {
    let mut state = TuiState::new(80, 24);
    let clock = Arc::new(rustain::domain::clock::MockClock::new(
        std::time::Instant::now(),
    ));
    state.notifications = rustain::adapters::tui::notifications::NotificationStore::from_config(
        &rustain::domain::models::NotificationsConfig::default(),
        clock.clone(),
    );
    let mut approvals = Approvals::new();
    let (id, mut response) = approvals.request(&mut state).await;
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Decision);
    assert_eq!(state.focus, FocusState::Input);
    assert!(rustain::adapters::tui::decision_queue::holds_request(
        &state, &id
    ));
    super::k3_compose::draw(
        &state,
        &super::k3_compose::layout(&state),
        Some("Decision: Permission"),
    );
    rustain::adapters::tui::decision_queue::note_painted(&mut state);
    clock.advance(rustain::adapters::tui::decision_queue::INERT_AFTER_PAINT);
    assert_eq!(press(&mut state, Key::Char('r')), InputAction::Consumed);
    let item = state.decisions.items().next().unwrap();
    let rustain::adapters::tui::decision_queue::DecisionSubject::Permission(payload) =
        &item.subject
    else {
        panic!("expected permission payload");
    };
    assert!(payload.reviewing);
    let action = press(&mut state, Key::Char('n'));
    assert_eq!(action, InputAction::PermissionDeny);
    approval_bridge::handle_action(&mut state, &action, &approvals.runtime).await;
    assert_eq!(
        response.try_recv().unwrap().outcome,
        ApprovalOutcome::Reject { feedback: None }
    );
    assert_eq!(approvals.runtime.rejected_count(), 1);
    assert!(state.decisions.is_empty());
    approvals.apply_resolution(&mut state, &id);
    assert_eq!(state.focus, FocusState::Input);
}

#[tokio::test]
async fn palette_action_close_leaves_the_real_request_queued() {
    for (action, notices_added, draft) in [
        (PaletteAction::ShowVersion, 1, ""),
        (
            PaletteAction::InsertMention("fixture.txt".into()),
            0,
            "@fixture.txt",
        ),
    ] {
        let mut state = TuiState::new(80, 24);
        assert_eq!(press(&mut state, Key::Ctrl('p')), InputAction::Consumed);
        // Real selected entries drive close -> dispatch, including an action
        // that requests Input AFTER close has already promoted the request.
        state.command_palette.filtered_entries = vec![PaletteEntry {
            name: "Selected action".into(),
            description: "Run selected action".into(),
            shortcut: None,
            scope: PaletteScope::All,
            action,
        }];
        let notices = state.notifications.records().count();
        let mut approvals = Approvals::new();
        let (id, mut response) = approvals.request(&mut state).await;
        assert_queued(&state);
        assert_eq!(press(&mut state, Key::Enter), InputAction::Consumed);
        assert_eq!(
            state.notifications.records().count(),
            notices + notices_added + 1
        );
        assert_eq!(state.input_buffer, draft);
        assert!(state.modal_stack.is_empty());
        assert_queued(&state);
        assert!(rustain::adapters::tui::decision_queue::holds_request(
            &state, &id
        ));
        assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(state.focus, FocusState::Input);
        assert_eq!(state.input_buffer, draft);
    }
}

#[tokio::test]
async fn surface_handoff_does_not_promote_a_card_in_the_transient_empty_stack() {
    for covered in [false, true] {
        let mut state = TuiState::new(80, 24);
        reducer::set_base(&mut state, FocusState::Chat);
        state.input_buffer = "kept through handoff".into();
        state.cursor_position = 2;
        if covered {
            for surface in [
                Surface::ModelSelector,
                Surface::ProfileSwitcher,
                Surface::UsagePanel,
            ] {
                reducer::open(&mut state, surface);
            }
        }
        reducer::open(&mut state, Surface::CommandPalette);
        let palette = state.modal_stack.top().unwrap().clone();
        let mut approvals = Approvals::new();
        let (id, mut response) = approvals.request(&mut state).await;
        assert_queued(&state);
        reducer::handoff(&mut state, Surface::CommandPalette, Surface::Help);
        let help = state.modal_stack.top().unwrap();
        assert_eq!(help.surface, Surface::Help);
        assert_ne!(help.id, palette.id);
        assert_eq!(state.modal_stack.len(), if covered { 4 } else { 1 });
        assert_eq!(state.focus, FocusState::Chat);
        assert_queued(&state);
        assert_eq!(press(&mut state, Key::Char('y')), InputAction::Consumed);
        assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
        while !state.modal_stack.is_empty() {
            assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
        }
        assert_eq!(state.focus, FocusState::Chat);
        assert_queued(&state);
        assert!(rustain::adapters::tui::decision_queue::holds_request(
            &state, &id
        ));
        assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(state.input_buffer, "kept through handoff");
        assert_eq!(state.cursor_position, 2);
    }
}

/// A state whose notifications run on a `MockClock`: the inert window is passed by
/// moving the clock, never by sleeping.
fn mock_state() -> (TuiState, Arc<MockClock>) {
    let clock = Arc::new(MockClock::new(std::time::Instant::now()));
    let mut state = TuiState::new(80, 24);
    state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    (state, clock)
}

/// The model-driven skill-trust producer the event loop's `SkillTrustPrompt` arm calls.
fn trust(state: &mut TuiState, name: &str) -> oneshot::Receiver<SkillTrustResponse> {
    let (response_tx, response) = oneshot::channel();
    decision_queue::raise_model_skill_trust(
        state,
        name.into(),
        std::path::PathBuf::from(format!("skills/{name}/SKILL.md")),
        response_tx,
    );
    response
}

/// DF-182 (6-C): trust prompts that arrive while the user is typing wait below Help — the
/// caller's frames and base are untouched — stay queued FIFO, and popping the last frame
/// never opens one by surprise; the user opens the oldest through the Needs-you list.
/// (Before 6-C the focus-only stage promoted the pending prompt as soon as the stack
/// emptied; the queue never opens a waiting item without the user.)
#[test]
fn skill_trust_under_help_retains_caller_queue_and_waits_after_pop() {
    use crate::conformance_21_6_decision_queue::support::{facts, paint, ready};
    for continued in [false, true] {
        let (mut state, clock) = mock_state();
        reducer::set_base(&mut state, FocusState::Chat);
        state.input_buffer = "untouched draft".into();
        state.cursor_position = 3;
        if continued {
            // Production reducer depth setup, matching k9_reconcile's suspended
            // Search fixture: covered openers need not be offered as key chords.
            for surface in [
                Surface::CommandPalette,
                Surface::ModelSelector,
                Surface::ProfileSwitcher,
            ] {
                reducer::open(&mut state, surface);
            }
        }
        assert_eq!(press(&mut state, Key::Ctrl('x')), InputAction::Consumed);
        assert_eq!(press(&mut state, Key::Char('?')), InputAction::Consumed);
        assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
        state.help_overlay.scroll_offset = 7;
        let retained = frames(&state);
        let caller = state.focus.clone();
        // RU-6: recent typing keeps an arrival queued instead of interrupting Help.
        state.last_printable_at = Some(state.notifications.now());
        let mut first_response = trust(&mut state, "first");
        let mut second_response = trust(&mut state, "second");
        assert_eq!(state.focus, caller);
        assert_eq!(frames(&state), retained);
        assert_eq!(state.decisions.undecided(), 2);
        assert_eq!(press(&mut state, Key::Char('y')), InputAction::Consumed);
        assert!(matches!(
            first_response.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(matches!(
            second_response.try_recv(),
            Err(TryRecvError::Empty)
        ));
        open_quit(&mut state);
        assert_eq!(state.modal_stack.len(), if continued { 4 } else { 2 });
        assert_eq!(
            state
                .modal_stack
                .iter()
                .any(|frame| frame.surface == Surface::Help),
            !continued,
        );
        assert!(presentation::is_open(&state, Surface::Help));
        assert_eq!(press(&mut state, Key::Char('n')), InputAction::Consumed);
        assert_eq!(frames(&state), retained);
        assert_eq!(state.help_overlay.scroll_offset, 7);
        assert_eq!(state.focus, caller);
        for (index, frame) in retained.iter().enumerate().rev() {
            assert_eq!(state.modal_stack.top(), Some(frame));
            assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
            assert_eq!(frames(&state), retained[..index]);
            if index > 0 {
                assert_eq!(
                    state.focus, caller,
                    "covered card cannot replace the retained base"
                );
            }
        }
        // Popping the final frame opens nothing: both prompts still wait, counted.
        assert!(state.modal_stack.is_empty());
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert_eq!(state.decisions.undecided(), 2);
        // The user opens the oldest through Ctrl+X q and Enter.
        decision_queue::open_waiting(&mut state, &LogFacts::default());
        assert_eq!(press(&mut state, Key::Enter), InputAction::Consumed);
        assert!(presentation::is_open(&state, Surface::Decision));
        paint(&mut state, &facts("c1"), 80, 24);
        ready(&mut state, &clock);
        let first = state.decisions.items().next().unwrap();
        let DecisionSubject::SkillTrust(first) = &first.subject else {
            panic!("skill trust first")
        };
        assert_eq!(first.skill_name, "first");
        assert_eq!(
            first.skill_file,
            std::path::PathBuf::from("skills/first/SKILL.md")
        );
        match &first.origin {
            SkillOrigin::Model { reply } => assert!(!reply.is_closed()),
            other => panic!("model-driven trust, got {other:?}"),
        }
        assert_eq!(state.decisions.count(DecisionKind::SkillTrust), 2);
        assert!(matches!(
            first_response.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(matches!(
            second_response.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert_eq!(
            press(&mut state, Key::Char('y')),
            InputAction::SkillTrustAccept
        );
        assert_eq!(state.input_buffer, "untouched draft");
        assert_eq!(state.cursor_position, 3);
        assert!(presentation::consistent(&state));
    }
}

#[test]
fn empty_stack_skill_trust_producer_is_an_independent_focus_control() {
    use crate::conformance_21_6_decision_queue::support::{facts, paint, ready};
    let (mut state, clock) = mock_state();
    let mut response = trust(&mut state, "uncovered");
    assert!(presentation::is_open(&state, Surface::Decision));
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, Key::Char('n')),
        InputAction::SkillTrustDecline
    );
    assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn queued_request_preserves_an_active_trust_owner() {
    use crate::conformance_21_6_decision_queue::support::{facts, paint, ready};
    let (mut state, clock) = mock_state();
    let mut approvals = Approvals::new();
    let mut trust_response = trust(&mut state, "active-owner");
    let open_kind =
        |state: &TuiState| decision_queue::current(state, &facts("c1")).map(|item| item.kind);
    assert_eq!(open_kind(&state), Some(DecisionKind::SkillTrust));
    let (first, mut first_response) = approvals.request(&mut state).await;
    assert_eq!(open_kind(&state), Some(DecisionKind::SkillTrust));
    let (second, mut second_response) = approvals.request(&mut state).await;
    assert_eq!(open_kind(&state), Some(DecisionKind::SkillTrust));
    assert!(rustain::adapters::tui::decision_queue::holds_request(
        &state, &first
    ));
    assert!(rustain::adapters::tui::decision_queue::holds_request(
        &state, &second
    ));
    assert_eq!(
        state.decisions.len(),
        3,
        "the trust prompt and two requests"
    );
    assert!(
        approvals
            .runtime
            .pending_review(&second, ReviewScope::AllTools)
            .await
            .is_some()
    );
    assert!(matches!(
        first_response.try_recv(),
        Err(TryRecvError::Empty)
    ));
    assert!(matches!(
        second_response.try_recv(),
        Err(TryRecvError::Empty)
    ));
    assert!(matches!(
        trust_response.try_recv(),
        Err(TryRecvError::Empty)
    ));
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, Key::Char('n')),
        InputAction::SkillTrustDecline
    );
}

#[tokio::test]
async fn queued_request_retains_a_deferred_export_until_its_owner_releases() {
    let mut state = TuiState::new(80, 24);
    let mut approvals = Approvals::new();
    let path = std::path::PathBuf::from("deferred-export.md");
    state.pending_export = Some((path.clone(), "retained export content".into()));
    reducer::raise_confirmation(&mut state, ConfirmationType::ExportOverwrite(path.clone()));
    let (first, mut first_response) = approvals.request(&mut state).await;
    let (second, mut second_response) = approvals.request(&mut state).await;
    assert_eq!(state.decisions.len(), 2);
    assert!(matches!(
        first_response.try_recv(),
        Err(TryRecvError::Empty)
    ));
    assert_eq!(
        state.focus,
        confirmation(ConfirmationType::ExportOverwrite(path.clone()))
    );
    let pending = state.pending_export.as_ref().unwrap();
    assert_eq!(pending.0, path);
    assert_eq!(pending.1, "retained export content");
    assert!(matches!(
        second_response.try_recv(),
        Err(TryRecvError::Empty)
    ));
    assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
    assert!(state.pending_export.is_some());
    assert_eq!(
        press(&mut state, Key::Char('n')),
        InputAction::CancelExportOverwrite
    );
    rustain::adapters::tui::handlers::export::apply_cancel_export_overwrite(&mut state);
    reducer::settle_legacy(&mut state);
    assert!(state.pending_export.is_none());
    assert!(rustain::adapters::tui::decision_queue::holds_request(
        &state, &first
    ));
    assert!(rustain::adapters::tui::decision_queue::holds_request(
        &state, &second
    ));
    assert_eq!(state.decisions.len(), 2);
    assert!(!presentation::is_open(&state, Surface::Decision));
    assert!(matches!(
        second_response.try_recv(),
        Err(TryRecvError::Empty)
    ));
}

/// A late resolution of either an active card or a queued card may clean its
/// payload, but must not rewrite the caller retained by covered/continued frames.
#[tokio::test]
async fn asynchronous_permission_resolution_preserves_covered_and_continued_base() {
    for active in [false, true] {
        for continued in [false, true] {
            let mut state = TuiState::new(80, 24);
            reducer::set_base(&mut state, FocusState::Chat);
            let mut approvals = Approvals::new();
            let request = if active {
                Some(approvals.request(&mut state).await)
            } else {
                None
            };
            let surfaces: &[Surface] = if continued && active {
                &[
                    Surface::CommandPalette,
                    Surface::ModelSelector,
                    Surface::Help,
                ]
            } else if continued {
                &[
                    Surface::CommandPalette,
                    Surface::ModelSelector,
                    Surface::ProfileSwitcher,
                    Surface::Help,
                ]
            } else {
                &[Surface::Help]
            };
            for &surface in surfaces {
                reducer::open(&mut state, surface);
            }
            let (id, mut response) = match request {
                Some(request) => request,
                None => approvals.request(&mut state).await,
            };
            if active {
                assert!(rustain::adapters::tui::decision_queue::holds_request(
                    &state, &id
                ));
            } else {
                assert_queued(&state);
            }
            let caller = state.focus.clone();
            let retained = frames(&state);
            open_quit(&mut state);
            let covered = frames(&state);
            assert_eq!(
                state.modal_stack.len(),
                if continued {
                    4
                } else if active {
                    3
                } else {
                    2
                }
            );
            assert_eq!(
                state.modal_stack.iter().any(|f| f.surface == Surface::Help),
                !continued
            );
            assert!(presentation::is_open(&state, Surface::Help));
            approvals
                .runtime
                .resolve(&id, ApprovalOutcome::Once, PermissionResolution::Operator)
                .await;
            assert_eq!(response.try_recv().unwrap().outcome, ApprovalOutcome::Once);
            approvals.apply_resolution(&mut state, &id);
            if active {
                assert_eq!(state.decisions.len(), 1);
                assert!(state.decisions.items().next().unwrap().is_stale());
            } else {
                assert!(state.decisions.is_empty());
            }
            assert_eq!(frames(&state), covered);
            assert_eq!(state.focus, caller);
            assert_eq!(press(&mut state, Key::Char('n')), InputAction::Consumed);
            assert_eq!(frames(&state), retained);
            assert_eq!(state.focus, caller);
            for (index, frame) in retained.iter().enumerate().rev() {
                assert_eq!(state.modal_stack.top(), Some(frame));
                assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
                assert_eq!(frames(&state), retained[..index]);
                if index > 0 {
                    assert_eq!(state.focus, caller);
                }
            }
            assert_eq!(state.focus, FocusState::Chat);
            assert!(
                approvals
                    .runtime
                    .pending_review(&id, ReviewScope::AllTools)
                    .await
                    .is_none()
            );
            assert!(presentation::consistent(&state));
        }
    }
}

/// AC18/A20 local witnesses deliberately use the production host draw, not
/// TestHarness::render (which is an alternate painter).
#[tokio::test]
async fn fully_occluded_block_is_not_presented() {
    local_occlusion::fully_occluded_block_is_not_presented().await;
}

mod local_occlusion {
    use super::*;
    use crate::conformance_tui_orchestration::render_runtime;
    use crate::e2e_harness::TestHarness;
    use arc_swap::ArcSwap;
    use clap::Parser;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Modifier};
    use rustain::adapters::noop::*;
    use rustain::adapters::tui::{
        decision_queue::{self, Arrival, DecisionDraft, DecisionKind},
        notifications::{self, NoticeSeverity, NoticeSource},
        views::{self, ViewKind},
        widgets::feedback_block,
    };
    use rustain::domain::models::{
        AppConfig, Direction, FeedbackLevel, JournalRecord, LogVisitCandidate, PeerId,
        RejectReason, RoomEvent, SandboxPolicy, TabManager,
    };
    use rustain::infrastructure::{
        composition::ComposeContext,
        runtime::{
            agent_core::AgentCore, app_state::AppState, event_bus::EventBus, transparency_bridge,
        },
        subagent::node_journal::{NodeJournal, WorkspaceJournalReader},
        transparency::TransparencyService,
    };
    use tokio_util::sync::CancellationToken;

    const LOG: &str = rustain::adapters::tui::handlers::team_command::TEAM_LOG_BLOCK_ID;

    struct Visit {
        h: TestHarness,
        tabs: TabManager,
        app: AppState,
        clock: Arc<MockClock>,
        _events: tokio::sync::mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>,
        _workspace: tempfile::TempDir,
    }

    // Same composition as transparency_bridge's durable visit fixture. None
    // of the noop ports produce the log: NodeJournal and TransparencyService do.
    fn app(
        workspace: &std::path::Path,
    ) -> (
        AppState,
        tokio::sync::mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>,
    ) {
        let sandbox_policy = Arc::new(tokio::sync::RwLock::new(SandboxPolicy::Permissive));
        let compose = Arc::new(ComposeContext {
            workspace_path: workspace.to_path_buf(),
            project_context: rustain::domain::models::project_context::ProjectContext::empty(),
            storage: Arc::new(NoOpStorage),
            skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
            domain_tx: None,
            channel_turn_tx: None,
            tool_exposure: "static-full".into(),
            assembler: "passthrough".into(),
            skill_exposure: "l1-metadata".into(),
            skill_cache: Arc::new(
                rustain::infrastructure::skill_cache::SkillCache::new_in_memory(),
            ),
            sandbox_adapter: "noop".into(),
            sandbox_startup_policy: SandboxPolicy::Permissive,
            sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
                rustain::adapters::sandbox::NoOpSandbox,
            )
                as Arc<dyn rustain::domain::ports::SandboxManager>)),
            memory_slot: Arc::new(ArcSwap::from_pointee(
                Arc::new(NoOpMemory) as Arc<dyn rustain::domain::ports::MemoryPort>
            )),
            sandbox_policy: sandbox_policy.clone(),
            memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
            peer_topic_store: Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
            #[cfg(feature = "meta-search")]
            search_config: Default::default(),
            #[cfg(feature = "meta-search")]
            meta_search_engine: None,
            a2a_peers: Vec::new(),
        });
        let (bus, events) = EventBus::new(16);
        let (mut app, events) = AppState::new(
            Arc::new(bus),
            events,
            ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence)),
            sandbox_policy,
            Arc::new(rustain::domain::services::plan_manager::PlanManager::new(
                workspace.to_path_buf(),
            )),
            Arc::new(rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new()),
            Arc::new(ArcSwap::from_pointee(
                Arc::new(NoOpProvider) as Arc<dyn rustain::domain::ports::StreamingProvider>
            )),
            Arc::new(rustain::adapters::provider::ProviderRegistry::new()),
            Arc::new(NoOpUsageLedger),
            Arc::new(rustain::adapters::budget::BudgetStateStore::new()),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(AgentCore::test_noop()),
            None,
            compose,
            Arc::new(ArcSwap::from_pointee(Arc::new(
                rustain::adapters::profile_resolver::noop::NoopProfileResolver,
            )
                as Arc<dyn rustain::domain::ports::ProfileResolver>)),
            rustain::adapters::cli::commands::Cli::try_parse_from(["rustain"]).unwrap(),
            None,
            rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
            #[cfg(feature = "meta-search")]
            None,
        );
        app.transparency = Some(Arc::new(TransparencyService::new(
            Arc::new(WorkspaceJournalReader::open_workspace(workspace)),
            workspace.to_path_buf(),
        )));
        (app, events)
    }

    impl Visit {
        async fn new(width: u16, height: u16) -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let journal = NodeJournal::open_workspace(workspace.path()).await.unwrap();
            journal
                .append_batch(
                    (0..2)
                        .map(|_| {
                            JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected {
                                rail: None,
                                peer: PeerId::from_public_key(&[4; 32]).unwrap(),
                                reason: RejectReason::Policy {
                                    detail: "K12-log-marker".into(),
                                },
                                direction: Direction::Inbound,
                                task: None,
                            })
                        })
                        .collect(),
                )
                .await
                .unwrap();
            let (app, events) = app(workspace.path());
            let mut h = TestHarness::with_size(width, height);
            // The 6-C kinds are scoped to conversation `c1` in their fixtures; the
            // notification store runs on a `MockClock` so the inert window is a clock move.
            h.conversation.id = "c1".into();
            let clock = Arc::new(MockClock::new(std::time::Instant::now()));
            h.state.notifications =
                NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
            let mut tabs = TabManager::new(CancellationToken::new());
            tabs.active_tab_mut().conversation = h.conversation.clone();
            h.state.active_tab_id = tabs.active_tab_id();
            h.state.focus = FocusState::Chat;
            // Read from the top so a block starting on the final screen row
            // is genuinely clipped to one row, not silently moved to bottom.
            tabs.active_tab_mut().view_state.scroll_offset = usize::MAX;
            tabs.active_tab_mut().view_state.mode = rustain::domain::models::AnchorMode::Reading;
            h.state.set_scroll_offset(usize::MAX);
            h.state.set_auto_scroll(false);
            Self {
                h,
                tabs,
                app,
                clock,
                _events: events,
                _workspace: workspace,
            }
        }

        /// The first paint started the inert clock: let the 500 ms window pass (no sleep).
        fn ready(&mut self) {
            decision_queue::note_painted(&mut self.h.state);
            self.clock.advance(decision_queue::INERT_AFTER_PAINT);
        }

        fn draw(&mut self) -> String {
            assert_eq!(
                self.h.conversation.id,
                self.tabs.active_tab().conversation.id
            );
            render_runtime(&mut self.h, &mut self.tabs, &self.app.agent_core)
        }

        async fn read(&mut self) -> LogVisitCandidate {
            transparency_bridge::team_command(
                &mut self.h.state,
                &self.tabs.active_tab().conversation.id,
                Some("log"),
                &self.app,
            )
            .await;
            let visit = self
                .h
                .state
                .pending_log_visit
                .expect("real unfiltered read arms visit");
            assert!(
                visit.seen_through >= 2,
                "snapshot came from the durable journal"
            );
            assert!(
                self.h.state.feedback_blocks[LOG]
                    .message
                    .contains("K12-log-marker")
            );
            visit
        }

        fn pane(&self) -> Rect {
            super::super::k3_compose::layout(&self.h.state).chat_pane
        }

        /// Produce and paint the log at an observed screen row. Compare EVERY
        /// projected line to the actual block renderer, including clipped ends.
        async fn place(&mut self, y: u16) -> Vec<u16> {
            let pane = self.pane();
            assert!((pane.y..pane.bottom()).contains(&y));
            self.h.state.feedback_blocks.clear();
            if y > pane.y {
                views::show_view(
                    &mut self.h.state.feedback_blocks,
                    ViewKind::TeamStatus,
                    "k12-padding",
                    FeedbackLevel::Info,
                    vec!["padding"; usize::from(y - pane.y)].join("\n"),
                );
            }
            let visit = self.read().await;
            let expected = feedback_block::render_feedback_lines(
                &self.h.state.feedback_blocks[LOG],
                pane.width,
                &self.h.state.theme,
            );
            assert!(
                expected.len() > 1,
                "independent multirow control is reachable"
            );
            self.draw();
            let rows: Vec<_> = (y..pane.bottom()).take(expected.len()).collect();
            assert!(!rows.is_empty());
            for (&row, line) in rows.iter().zip(&expected) {
                let text: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                assert!(
                    self.row(row).starts_with(&text),
                    "actual projected feedback {row}: {:?}, expected {text:?}",
                    self.row(row),
                );
            }
            self.acknowledged(visit);
            rows
        }

        fn row(&self, y: u16) -> String {
            let pane = self.pane();
            super::super::k3_compose::row(self.h.terminal.backend().buffer(), pane, y)
        }

        fn acknowledged(&self, visit: LogVisitCandidate) {
            assert_eq!(self.h.state.pending_log_visit, None);
            assert_eq!(self.h.state.log_awareness.presented.last(), Some(&visit));
        }

        fn retained(&self, visit: LogVisitCandidate, presented: usize) {
            assert_eq!(
                self.h.state.pending_log_visit,
                Some(visit),
                "M12: actual opaque paint covered every projected feedback row; the visit must remain pending",
            );
            assert_eq!(self.h.state.log_awareness.presented.len(), presented);
        }

        /// Recover geometry from actual painted rails, not a second copy of
        /// the host's layout arithmetic. Require both closing corners.
        fn painted_box(&self, title: &str) -> Rect {
            let buffer = self.h.terminal.backend().buffer();
            for y in buffer.area.y..buffer.area.bottom() {
                let text = super::super::k3_compose::row(buffer, buffer.area, y);
                if !text.contains(title) {
                    continue;
                }
                let x = (0..buffer.area.width)
                    .find(|&x| buffer[(x, y)].symbol() == "┌")
                    .expect("painted opening rail");
                let right = (x + 1..buffer.area.right())
                    .find(|&x| buffer[(x, y)].symbol() == "┐")
                    .expect("painted top right");
                let bottom = (y + 1..buffer.area.bottom())
                    .find(|&bottom| {
                        buffer[(x, bottom)].symbol() == "└"
                            && buffer[(right, bottom)].symbol() == "┘"
                    })
                    .expect("painted closing rail");
                return Rect::new(x, y, right - x + 1, bottom - y + 1);
            }
            panic!("actual draw did not paint {title:?}");
        }
    }

    /// M12: a narrow centered frame still covers the ENTIRE logical row for
    /// acknowledgement. Surviving text to its left is not a visible-row escape.
    pub(super) async fn fully_occluded_block_is_not_presented() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut f = Visit::new(width, height).await;
            f.place(f.pane().y).await;
            press(&mut f.h.state, Key::Ctrl('x'));
            press(&mut f.h.state, Key::Char('?'));
            f.draw();
            let rect = f.painted_box("Help");
            assert_eq!(press(&mut f.h.state, Key::Esc), InputAction::Consumed);
            let rows = f.place(rect.y.max(f.pane().y)).await;
            assert!(rows.iter().all(|&y| y >= rect.y && y < rect.bottom()));
            let left: Vec<_> = rows
                .iter()
                .map(|&y| {
                    f.h.terminal.backend().buffer()[(f.pane().x, y)]
                        .symbol()
                        .to_owned()
                })
                .collect();
            let visit = f.read().await;
            let presented = f.h.state.log_awareness.presented.len();
            press(&mut f.h.state, Key::Ctrl('x'));
            press(&mut f.h.state, Key::Char('?'));
            f.draw();
            assert_eq!(f.painted_box("Help"), rect);
            assert!(rect.x < f.pane().right() && rect.right() > f.pane().x);
            if width == 130 {
                assert!(rect.x > f.pane().x && rect.right() < f.pane().right());
                for (&y, marker) in rows.iter().zip(&left) {
                    assert_eq!(
                        f.h.terminal.backend().buffer()[(f.pane().x, y)].symbol(),
                        marker
                    );
                }
            }
            f.retained(visit, presented);
            press(&mut f.h.state, Key::Esc);
            f.draw();
            f.acknowledged(visit);
        }
    }

    #[tokio::test]
    async fn streaming_indicator_does_not_present_an_overwritten_log_row() {
        for (streaming, surviving) in [(true, false), (true, true), (false, false)] {
            let mut f = Visit::new(80, 24).await;
            let last = f.pane().bottom() - 1;
            // Observe the actual legacy streaming prefix before placing the
            // feedback. Merely toggling streaming after place shifts the block
            // entirely offscreen and would make a retention assertion vacuous.
            f.h.streaming.is_streaming = streaming;
            f.draw();
            let prefix = if streaming {
                u16::try_from(f.h.state.total_content_height).unwrap()
                    + f.h.state.theme.spacing.normal
            } else {
                0
            };
            f.h.streaming.is_streaming = false;
            let y = last - u16::from(surviving);
            let rows = f.place(y - prefix).await;
            assert_eq!(rows[0] + prefix, y);
            let first = f.row(rows[0]);
            let visit = f.read().await;
            let presented = f.h.state.log_awareness.presented.len();
            f.h.streaming.is_streaming = streaming;
            f.draw();
            if streaming {
                assert!(f.row(last).contains("New content below"));
            } else {
                assert_eq!(f.row(last), first);
            }
            if streaming && !surviving {
                f.retained(visit, presented);
                f.h.streaming.is_streaming = false;
                f.draw();
                f.acknowledged(visit);
            } else {
                if surviving {
                    assert_eq!(f.row(y), first);
                }
                f.acknowledged(visit);
            }
        }
    }

    // Payload constructors mirror the existing K11 producer-shaped fixtures. Only
    // event-backed kinds may ARRIVE beneath an already-open frame.
    //
    // 21-6 slice 6-C moved the skill-trust, plan approval / card, delegation, spawn-gate
    // and question cards into the decision queue. This matrix keeps the cards 6-D has yet
    // to move; the moved kinds are covered pairwise by
    // `queue_kinds_paint_only_the_oldest_item_and_decide_it_alone`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Mixed {
        Apply,
        Consolidation,
        Forget,
        Export,
        Peer,
        Retract,
        Fork,
        Rewind,
    }

    const MIXED: [Mixed; 8] = [
        Mixed::Apply,
        Mixed::Consolidation,
        Mixed::Forget,
        Mixed::Export,
        Mixed::Peer,
        Mixed::Retract,
        Mixed::Fork,
        Mixed::Rewind,
    ];
    const ASYNC: [Mixed; 2] = [Mixed::Consolidation, Mixed::Retract];

    fn seed_mixed(f: &mut Visit, kind: Mixed) {
        use crate::conformance_21_11_display_width::k5b_cards::{
            apply_pending, peer_pending, retract_pending,
        };
        use rustain::adapters::tui::state::*;
        let state = &mut f.h.state;
        let confirmation = match kind {
            Mixed::Apply => {
                state.pending_artifact_card = Some(apply_pending(vec!["mixed.rs".into()], false));
                Some(ConfirmationType::ArtifactApply)
            }
            Mixed::Consolidation => {
                state.pending_consolidation_card = Some(PendingConsolidationCard {
                    conversation_id: "c1".into(),
                    proposals: vec![],
                });
                reducer::take_chat_focus_for_card(state, true);
                None
            }
            Mixed::Forget => {
                state.pending_forget_card = Some(PendingForgetCard {
                    conversation_id: "c1".into(),
                    candidates: vec![],
                    focused_index: 0,
                });
                None
            }
            Mixed::Export => {
                state.pending_export = Some(("mixed-export.md".into(), "retained export".into()));
                Some(ConfirmationType::ExportOverwrite("mixed-export.md".into()))
            }
            Mixed::Peer => {
                state.pending_peer_add = Some(peer_pending("mixed-peer", None));
                Some(ConfirmationType::PeerAdd)
            }
            Mixed::Retract => {
                state.pending_team_retract = Some(retract_pending(
                    "mixed retract body\n\nAwaiting your decision.  [y] Retract  [n] Cancel",
                ));
                Some(ConfirmationType::TeamRetract)
            }
            Mixed::Fork => {
                state.pending_fork_index = Some(0);
                Some(ConfirmationType::Fork)
            }
            Mixed::Rewind => {
                state.pending_rewind_index = Some(0);
                state.rewind_preview = Some(RewindPreview {
                    target_message_index: 0,
                    messages_to_remove: 1,
                    files_to_revert: vec![],
                });
                Some(ConfirmationType::Rewind)
            }
        };
        if let Some(kind) = confirmation {
            reducer::raise_confirmation(state, kind);
        }
    }

    fn selected_mixed(state: &TuiState) -> Mixed {
        match &state.focus {
            FocusState::Overlay(OverlayType::Confirmation(kind)) => match kind {
                ConfirmationType::ArtifactApply => Mixed::Apply,
                ConfirmationType::ExportOverwrite(_) => Mixed::Export,
                ConfirmationType::PeerAdd => Mixed::Peer,
                ConfirmationType::TeamRetract => Mixed::Retract,
                ConfirmationType::Fork => Mixed::Fork,
                ConfirmationType::Rewind => Mixed::Rewind,
            },
            FocusState::Chat => {
                if state.pending_forget_card.is_some() {
                    Mixed::Forget
                } else {
                    assert!(state.pending_consolidation_card.is_some());
                    Mixed::Consolidation
                }
            }
            other => panic!("mixed fixture lost its owner: {other:?}"),
        }
    }

    fn offered_rails(f: &Visit) -> Vec<String> {
        let keys = [
            "[y]", "[n]", "[d]", "[c]", "[Enter]", "[e]", "[a]", "[s]", "[f]", "[r]",
        ];
        (f.pane().y..f.pane().bottom())
            .map(|y| f.row(y))
            .filter(|row| keys.iter().any(|key| row.contains(key)))
            .collect()
    }

    fn open_mixed_door(state: &mut TuiState, surface: Surface) -> bool {
        if surface == Surface::QuitConfirm {
            let action = press(state, Key::Ctrl('q'));
            if action != InputAction::Quit {
                assert!(
                    state.modal_stack.is_empty(),
                    "legacy CtrlQ does not open a frame"
                );
                return false;
            }
            use rustain::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost, QuitRequest};
            assert_eq!(
                quit_confirm::request(
                    state,
                    QuitFacts {
                        streams: 1,
                        ..QuitFacts::idle(QuitHost::Local)
                    }
                ),
                QuitRequest::Confirming
            );
        } else {
            press(state, Key::Ctrl('x'));
            if state.pending_leader.is_none() {
                assert!(
                    state.modal_stack.is_empty(),
                    "legacy card does not open leader"
                );
                return false;
            }
            assert_eq!(
                press(
                    state,
                    Key::Char(if surface == Surface::Help { '?' } else { 'n' })
                ),
                InputAction::Consumed
            );
        }
        assert_eq!(state.modal_stack.top().unwrap().surface, surface);
        true
    }

    #[tokio::test]
    async fn c_r1_reachable_mixed_ordered_pairs_paint_only_the_selected_owner() {
        let mut reached = [0; 3];
        for (surface_index, surface) in [
            Surface::QuitConfirm,
            Surface::NotificationLog,
            Surface::Help,
        ]
        .into_iter()
        .enumerate()
        {
            for first in MIXED {
                for second in ASYNC {
                    if first == second {
                        continue;
                    }
                    let mut f = Visit::new(130, 40).await;
                    // Log/Help cannot be opened FROM an overlay card. Their
                    // genuine mixed case is two asynchronous arrivals below it.
                    if surface != Surface::QuitConfirm {
                        if !ASYNC.contains(&first) {
                            continue;
                        }
                        assert!(open_mixed_door(&mut f.h.state, surface));
                    }
                    seed_mixed(&mut f, first);
                    if surface == Surface::QuitConfirm && !open_mixed_door(&mut f.h.state, surface)
                    {
                        continue;
                    }
                    let retained = f.h.state.focus.clone();
                    seed_mixed(&mut f, second);
                    assert_eq!(f.h.state.focus, retained, "{first:?} -> {second:?}");
                    assert_eq!(press(&mut f.h.state, Key::Esc), InputAction::Consumed);
                    let selected = selected_mixed(&f.h.state);
                    if matches!(retained, FocusState::Overlay(_)) {
                        assert_eq!(f.h.state.focus, retained, "keep the selected real owner");
                        assert_eq!(selected, first);
                    }
                    f.draw();
                    let mut solo = Visit::new(130, 40).await;
                    seed_mixed(&mut solo, selected);
                    solo.draw();
                    assert!(
                        !offered_rails(&solo).is_empty(),
                        "solo {selected:?} genuinely offers keys"
                    );
                    assert_eq!(
                        offered_rails(&f),
                        offered_rails(&solo),
                        "{surface:?}: {first:?} -> {second:?}, selected {selected:?}"
                    );
                    // A raw decision key has the same meaning as the solo paint,
                    // without running any effect for the deferred operation.
                    let key = Key::Char('y');
                    assert_eq!(press(&mut f.h.state, key), press(&mut solo.h.state, key));
                    reached[surface_index] += 1;
                }
            }
        }
        // Four legacy Quit doors stay closed (Forget's special-key consumer, Export, Fork,
        // Rewind): 4 open first cards x 2 async seconds - 2 equal pairs; Log/Help admit
        // the 2 ordered pairs of the 2 async cards each.
        assert_eq!(reached, [6, 2, 2], "all reachable ordered pairs must run");
    }

    #[tokio::test]
    async fn c_r1_legacy_non_openable_doors_remain_closed() {
        for kind in [Mixed::Forget, Mixed::Export, Mixed::Fork, Mixed::Rewind] {
            let mut f = Visit::new(80, 24).await;
            seed_mixed(&mut f, kind);
            let focus = f.h.state.focus.clone();
            assert!(
                !open_mixed_door(&mut f.h.state, Surface::QuitConfirm),
                "{kind:?}"
            );
            assert_eq!(f.h.state.focus, focus);
            assert!(!open_mixed_door(&mut f.h.state, Surface::Help), "{kind:?}");
            assert_eq!(f.h.state.focus, focus);
        }
        // Permission is now a stack frame, not a legacy non-openable card; since 6-C so is
        // every question / trust / plan / delegation / spawn-gate / kill card: `Ctrl+Q`
        // requests quit from their frame (see
        // `queue_kinds_request_quit_from_their_frame_unlike_the_legacy_cards`).
    }

    #[tokio::test]
    async fn c_r1_real_permission_and_async_cards_share_the_painted_owner() {
        for surface in [
            Surface::QuitConfirm,
            Surface::NotificationLog,
            Surface::Help,
        ] {
            for kind in ASYNC {
                let mut f = Visit::new(130, 40).await;
                assert!(open_mixed_door(&mut f.h.state, surface));
                let mut approvals = Approvals::new();
                let (id, mut response) = approvals.request(&mut f.h.state).await;
                seed_mixed(&mut f, kind);
                assert_eq!(press(&mut f.h.state, Key::Esc), InputAction::Consumed);
                f.draw();
                assert_queued(&f.h.state);
                assert!(decision_queue::holds_request(&f.h.state, &id));
                let mut solo = Visit::new(130, 40).await;
                let kind = selected_mixed(&f.h.state);
                seed_mixed(&mut solo, kind);
                solo.draw();
                let actual = offered_rails(&f);
                let expected = offered_rails(&solo);
                assert_eq!(
                    actual.len(),
                    expected.len(),
                    "{surface:?}: permission + {kind:?}"
                );
                for (actual, expected) in actual.iter().zip(&expected) {
                    // A waiting-decision toast can share this terminal row, outside
                    // the card. Compare the complete card rail, not the toast.
                    let expected = expected.trim_end_matches(['│', '║', '╝', '┘', '═', '─', ' ']);
                    let suffix = actual.strip_prefix(expected).unwrap_or_else(|| {
                        panic!("{surface:?}: permission + {kind:?}: {actual:?} != {expected:?}")
                    });
                    for key in [
                        "[y]", "[n]", "[d]", "[c]", "[Enter]", "[e]", "[a]", "[s]", "[f]", "[r]",
                    ] {
                        assert!(!suffix.contains(key), "unexpected second rail: {actual}");
                    }
                }
                assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
            }
        }
    }

    /// 6-C: every ordered pair of the moved kinds (the first item of any kind, a second
    /// automatic arrival of any automatic kind): ONE frame for two items, painted rail ==
    /// the solo rail of the OLDEST, its deciding key means what it means solo, and the
    /// other item is untouched and still counted.
    #[tokio::test]
    async fn queue_kinds_paint_only_the_oldest_item_and_decide_it_alone() {
        use crate::conformance_21_6_decision_queue::raisers::{
            ALL_6C, PLAIN, deciding, press as frame_press, raise, raise_second_in,
        };
        // Requested kinds (user-started) open at once; only automatic arrivals queue behind.
        let automatic = [
            DecisionKind::Question,
            DecisionKind::SkillTrust,
            DecisionKind::PlanApproval,
            DecisionKind::PlanCard,
            DecisionKind::PlanDeviation,
            DecisionKind::Delegation,
        ];
        let mut reached = 0;
        for first in ALL_6C {
            let mut solo = Visit::new(130, 40).await;
            let _solo_hold = raise(&mut solo.h.state, first, PLAIN);
            solo.draw();
            let solo_rails = offered_rails(&solo);
            assert!(
                !solo_rails.is_empty(),
                "solo {first:?} genuinely offers keys"
            );
            solo.ready();
            let solo_action = frame_press(&mut solo.h.state, deciding(first));
            for second in automatic {
                let mut f = Visit::new(130, 40).await;
                let _first_hold = raise(&mut f.h.state, first, PLAIN);
                // Both receivers stay alive: a dropped question receiver is a gone turn (stale).
                let _second_hold = if second == first {
                    raise_second_in(&mut f.h.state, second, "c1")
                } else {
                    raise(&mut f.h.state, second, PLAIN).1
                };
                let at = format!("{first:?} -> {second:?}");
                assert_eq!(f.h.state.decisions.undecided(), 2, "{at}");
                assert_eq!(
                    f.h.state
                        .modal_stack
                        .iter()
                        .filter(|frame| frame.surface == Surface::Decision)
                        .count(),
                    1,
                    "{at}: one frame for two items"
                );
                f.draw();
                assert_eq!(offered_rails(&f), solo_rails, "{at}");
                f.ready();
                assert_eq!(
                    frame_press(&mut f.h.state, deciding(first)),
                    solo_action,
                    "{at}"
                );
                assert!(
                    f.h.state.decisions.holds(second),
                    "{at}: the other item waits"
                );
                reached += 1;
            }
        }
        assert_eq!(reached, 60, "every ordered pair ran");
    }

    /// 6-C: the legacy overlay cards swallow `Ctrl+Q` (DF-21-3-LOCAL-CARD-CTRLQ); the moved
    /// kinds are frames, whose parent row requests quit and leaves the item queued.
    #[test]
    fn queue_kinds_request_quit_from_their_frame_unlike_the_legacy_cards() {
        use crate::conformance_21_6_decision_queue::raisers::{ALL_6C, PLAIN, open_ready};
        use crate::conformance_21_6_decision_queue::support::{facts, route};
        use rustain::adapters::tui::presentation::RouteEffect;
        use rustain::domain::events::{DomainInputEvent, DomainKey};
        for kind in ALL_6C {
            let (mut state, _clock, _hold) = open_ready(kind, PLAIN);
            assert_eq!(
                route(
                    &mut state,
                    &DomainInputEvent::SpecialKey(DomainKey::CtrlQ),
                    &facts("c1")
                ),
                Some(RouteEffect::QuitRequest),
                "{kind:?}"
            );
            assert!(
                state.decisions.holds(kind),
                "{kind:?}: quitting never decides"
            );
        }
    }

    #[tokio::test]
    async fn c_r1_agent_kill_is_retained_without_painting_over_the_promoted_card() {
        use super::super::keymap_support::decode;
        use crate::conformance_21_6_decision_queue::raisers::press as frame_press;
        use rustain::adapters::tui::agent_inspector_keys::{self, InspectorOutcome};
        use rustain::domain::models::visual::PanelType;
        use rustain::domain::models::{AgentId, AgentRowView, NodeState, OwnershipKind};
        let mut f = Visit::new(130, 40).await;
        let id = AgentId::parse("spoke-1").unwrap();
        f.h.state.sidebar_visible = true;
        f.h.state.sidebar_panel = Some(PanelType::Agents);
        reducer::set_base(
            &mut f.h.state,
            FocusState::Sidebar {
                panel: PanelType::Agents,
                selected: 0,
            },
        );
        f.h.state.agent_panel_state.cached_entries = vec![AgentRowView {
            isolated: false,
            agent_id: id.clone(),
            parent_id: AgentId::root(),
            subagent_type: "mixed-kill-agent".into(),
            spawned_at: 0,
            depth: 1,
            current_status: NodeState::Running,
            ownership: OwnershipKind::Owned,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            wait_reason: None,
        }];
        f.h.state.agent_panel_state.drill_down_agent = Some(id.clone());
        let conversation_id = f.h.conversation.id.clone();
        assert!(matches!(
            agent_inspector_keys::on_key(
                &mut f.h.state,
                &decode(Key::Char('x')).unwrap(),
                false,
                &conversation_id,
            ),
            InspectorOutcome::Consumed
        ));
        // The kill confirmation is a decision frame naming the agent.
        assert!(f.draw().contains("mixed-kill-agent"));
        assert!(presentation::is_open(&f.h.state, Surface::Decision));
        // A trust prompt arrives while the kill frame is open: it waits behind it.
        let mut response = trust(&mut f.h.state, "mixed-trust");
        assert_eq!(f.h.state.decisions.undecided(), 2);
        let screen = f.draw();
        assert!(screen.contains("mixed-kill-agent"), "{screen}");
        assert!(!screen.contains("mixed-trust"), "{screen}");
        // The user sets the kill aside: it stays retained and counted, is not painted, and
        // the waiting trust prompt is NOT opened by surprise.
        f.ready();
        assert_eq!(frame_press(&mut f.h.state, Key::Esc), InputAction::Consumed);
        let screen = f.draw();
        assert!(!screen.contains("and its subtree?"), "{screen}");
        assert!(!screen.contains("mixed-trust"), "{screen}");
        assert!(f.h.state.decisions.holds(DecisionKind::AgentKill));
        // The user opens the trust prompt through the list: it paints, the kill does not.
        decision_queue::open_waiting(&mut f.h.state, &LogFacts::default());
        assert_eq!(press(&mut f.h.state, Key::Enter), InputAction::Consumed);
        let screen = f.draw();
        assert!(screen.contains("mixed-trust"), "{screen}");
        assert!(!screen.contains("and its subtree?"), "{screen}");
        assert!(f.h.state.decisions.holds(DecisionKind::AgentKill));
        assert!(matches!(
            agent_inspector_keys::on_key(
                &mut f.h.state,
                &decode(Key::Char('y')).unwrap(),
                false,
                &conversation_id,
            ),
            InspectorOutcome::PassThrough
        ));
        f.ready();
        assert_eq!(
            frame_press(&mut f.h.state, Key::Char('y')),
            InputAction::SkillTrustAccept
        );
        assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
        assert!(f.h.state.decisions.holds(DecisionKind::AgentKill));
    }

    #[tokio::test]
    async fn c_r1_actual_artifact_quit_trust_executes_only_the_visible_apply_once() {
        use crate::conformance_21_6_decision_queue::raisers::press as frame_press;
        use rustain::adapters::tui::handlers::artifact_command::resolve_apply_card;
        use rustain::adapters::{artifact::FileSystemArtifactStore, merge_back::GitPatchApplier};
        use rustain::domain::models::{
            AgentId, CapabilityTokenId, HostBinding, PermissionMode, ProvenanceTag,
            ProvisioningTier, UnifiedDiff,
        };
        use rustain::domain::services::patch_review::MergeBackPolicy;
        use rustain::infrastructure::{
            orchestrator::{JournalPatchReview, PatchMergeBack},
            runtime::artifact_bridge,
        };
        let mut f = Visit::new(130, 40).await;
        let root = f._workspace.path();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        let journal = Arc::new(NodeJournal::open_workspace(root).await.unwrap());
        let store = Arc::new(FileSystemArtifactStore::new(root));
        let merge = Arc::new(PatchMergeBack::new(
            root.to_path_buf(),
            store.clone(),
            journal,
            f.app.event_bus.clone(),
            Arc::new(GitPatchApplier),
        ));
        let artifact = merge.capture(
            AgentId::parse("spoke-1").unwrap(), CapabilityTokenId::root(),
            vec![ProvenanceTag::SelfOriginated], vec![],
            HostBinding::new(rustain::infrastructure::subagent::current_host_id(root), "ws"),
            &UnifiedDiff::new(ProvisioningTier::ScratchCopy,
                "diff --git a/visible.txt b/visible.txt\nnew file mode 100644\n--- /dev/null\n+++ b/visible.txt\n@@ -0,0 +1 @@\n+visible operation\n".into()),
        ).await.unwrap();
        f.app.patch_review = Some(Arc::new(JournalPatchReview::new(
            merge.clone(),
            store,
            MergeBackPolicy::default(),
        )));
        f.app.patch_apply = Some(merge);
        let conversation_id = f.h.conversation.id.clone();
        artifact_bridge::artifact_command(
            &mut f.h.state,
            &conversation_id,
            Some(&format!("review {} approve", artifact.id.as_str())),
            &f.app,
            PermissionMode::Yolo,
        )
        .await;
        artifact_bridge::artifact_command(
            &mut f.h.state,
            &conversation_id,
            Some(&format!("apply {}", artifact.id.as_str())),
            &f.app,
            PermissionMode::Yolo,
        )
        .await;
        assert_eq!(
            f.h.state.focus,
            confirmation(ConfirmationType::ArtifactApply)
        );
        assert!(f.h.state.pending_artifact_card.is_some());
        assert!(f.draw().contains("[y] Apply"));
        open_quit(&mut f.h.state);
        // The trust prompt and a permission arrive under the quit frame: both queue.
        let mut response = trust(&mut f.h.state, "deferred-real-trust");
        let mut approvals = Approvals::new();
        let (_, mut permission_response) = approvals.request(&mut f.h.state).await;
        assert_eq!(f.h.state.decisions.len(), 2);
        assert_eq!(press(&mut f.h.state, Key::Char('n')), InputAction::Consumed);
        let screen = f.draw();
        assert!(screen.contains("[y] Apply"), "{screen}");
        assert!(!screen.contains("deferred-real-trust"), "{screen}");
        assert!(!f._workspace.path().join("visible.txt").exists());
        assert_eq!(
            press(&mut f.h.state, Key::Char('y')),
            InputAction::ApplyCardAccept
        );
        let card = resolve_apply_card(&mut f.h.state, true).unwrap();
        artifact_bridge::apply_confirmed_card(&mut f.h.state, &f.app, card, PermissionMode::Yolo)
            .await;
        reducer::settle_legacy(&mut f.h.state);
        assert_eq!(
            std::fs::read_to_string(f._workspace.path().join("visible.txt")).unwrap(),
            "visible operation\n"
        );
        assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(
            permission_response.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(
            resolve_apply_card(&mut f.h.state, true).is_none(),
            "spent apply cannot repeat"
        );
        // The settled apply card never opens the queued trust prompt by surprise; the user
        // opens the oldest waiting decision through the list.
        assert!(!presentation::is_open(&f.h.state, Surface::Decision));
        assert_eq!(f.h.state.decisions.undecided(), 2);
        decision_queue::open_waiting(&mut f.h.state, &LogFacts::default());
        assert_eq!(press(&mut f.h.state, Key::Enter), InputAction::Consumed);
        assert!(f.draw().contains("deferred-real-trust"));
        f.ready();
        let waiting =
            f.h.state
                .decisions
                .items()
                .find_map(|item| match &item.subject {
                    DecisionSubject::SkillTrust(payload) => Some((**payload).clone()),
                    _ => None,
                })
                .expect("the trust prompt is still queued");
        assert_eq!(
            frame_press(&mut f.h.state, Key::Char('y')),
            InputAction::SkillTrustAccept
        );
        // The event-loop arm hands the answer to the waiting turn, exactly once.
        assert!(waiting.reply_model(SkillTrustResponse::Accepted));
        assert!(!waiting.reply_model(SkillTrustResponse::Accepted));
        assert_eq!(response.try_recv().unwrap(), SkillTrustResponse::Accepted);
        assert_ne!(
            press(&mut f.h.state, Key::Char('y')),
            InputAction::ApplyCardAccept
        );
        assert!(f.h.state.pending_artifact_card.is_none());
    }

    #[tokio::test]
    async fn one_uncovered_projected_row_acknowledges_despite_inline_search() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut f = Visit::new(width, height).await;
            let rows = f.place(f.pane().y).await;
            assert!(rows.len() >= 2);
            let last = f.row(*rows.last().unwrap());
            let visit = f.read().await;
            assert_eq!(press(&mut f.h.state, Key::Char('i')), InputAction::Consumed);
            assert_eq!(f.h.state.focus, FocusState::Input);
            let action = press(&mut f.h.state, Key::Ctrl('f'));
            assert_eq!(action, InputAction::OpenSearch);
            rustain::adapters::tui::handlers::search::handle_search_action(
                &mut f.tabs,
                &mut f.h.state,
                &f.h.conversation,
                &action,
            );
            assert_eq!(
                f.h.state.modal_stack.top().unwrap().surface,
                Surface::Search
            );
            let screen = f.draw();
            assert!(screen.contains("Search:"));
            assert_ne!(f.row(rows[0]).chars().next(), Some('│'));
            assert_eq!(f.row(*rows.last().unwrap()), last);
            f.acknowledged(visit);
        }
    }

    #[tokio::test]
    async fn absent_and_not_painted_log_blocks_do_not_acknowledge() {
        for too_small in [false, true] {
            let mut f = Visit::new(80, 24).await;
            f.place(f.pane().y).await;
            let visit = f.read().await;
            let presented = f.h.state.log_awareness.presented.len();
            if too_small {
                f.h.terminal = Terminal::new(TestBackend::new(59, 15)).unwrap();
            } else {
                f.h.state.feedback_blocks.remove(LOG).unwrap();
            }
            let screen = f.draw();
            assert!(!screen.contains("K12-log-marker"));
            f.retained(visit, presented);
            if too_small {
                f.h.terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            } else {
                // A fresh real read, not injected acknowledgement identifiers.
                f.read().await;
            }
            f.draw();
            f.acknowledged(visit);
        }
    }

    #[tokio::test]
    async fn which_key_and_decision_frame_cover_the_actual_clipped_row() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            for decision in [false, true] {
                let mut f = Visit::new(width, height).await;
                let clock = Arc::new(rustain::domain::clock::MockClock::new(
                    std::time::Instant::now(),
                ));
                f.h.state.notifications = notifications::NotificationStore::from_config(
                    &rustain::domain::models::NotificationsConfig::default(),
                    clock.clone(),
                );
                // old -> new: the decision pane was bottom-anchored over the chat pane, so
                // the chat pane's last row was always under it; the frame is the Standard
                // `frame_rect` (a margin around it at >= 100 columns), so the decision
                // case places its row on the frame's own last row.
                let y = if decision {
                    use rustain::adapters::tui::presentation::geometry;
                    let layout = super::super::k3_compose::layout(&f.h.state);
                    let region = geometry::modal_region(&layout, Rect::new(0, 0, width, height));
                    // The projected block is three rows: it must lie wholly inside the frame.
                    geometry::frame_rect(region).bottom() - 3
                } else {
                    f.pane().bottom() - 1
                };
                assert_eq!(f.place(y).await[0], y);
                let visit = f.read().await;
                let presented = f.h.state.log_awareness.presented.len();
                if decision {
                    decision_queue::raise(
                        &mut f.h.state,
                        DecisionDraft::new(DecisionKind::Budget, "K12 actual decision"),
                        Arrival::Requested,
                    );
                    assert_eq!(
                        f.h.state.modal_stack.top().unwrap().surface,
                        Surface::Decision
                    );
                } else {
                    press(&mut f.h.state, Key::Ctrl('x'));
                    assert!(f.h.state.pending_leader.is_some());
                }
                f.draw();
                let rect = f.painted_box(if decision { "Daily budget" } else { "Ctrl+X" });
                assert!(rect.y <= y && y < rect.bottom());
                assert!(rect.x < f.pane().right() && rect.right() > f.pane().x);
                if decision && width >= 100 {
                    // old -> new: the bottom-anchored pane left no uncovered feedback row under
                    // its box; the centered frame leaves the rows above it visible, so the
                    // visit is genuinely presented (the 60/80 sheets still hide every row).
                    f.acknowledged(visit);
                } else {
                    f.retained(visit, presented);
                }
                if decision {
                    clock.advance(decision_queue::INERT_AFTER_PAINT);
                }
                assert_eq!(press(&mut f.h.state, Key::Esc), InputAction::Consumed);
                assert!(f.h.state.modal_stack.is_empty());
                assert!(f.h.state.pending_leader.is_none());
                f.draw();
                f.acknowledged(visit);
            }
        }
    }

    #[tokio::test]
    async fn permission_frame_covers_the_actual_clipped_row() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut f = Visit::new(width, height).await;
            let clock = Arc::new(rustain::domain::clock::MockClock::new(
                std::time::Instant::now(),
            ));
            f.h.state.notifications = notifications::NotificationStore::from_config(
                &rustain::domain::models::NotificationsConfig::default(),
                clock.clone(),
            );
            let layout = super::super::k3_compose::layout(&f.h.state);
            let region = rustain::adapters::tui::presentation::geometry::modal_region(
                &layout,
                Rect::new(0, 0, width, height),
            );
            let y = rustain::adapters::tui::presentation::geometry::frame_rect(region).bottom() - 3;
            assert_eq!(f.place(y).await[0], y);
            let visit = f.read().await;
            let presented = f.h.state.log_awareness.presented.len();
            let mut approvals = Approvals::new();
            let (id, mut response) = approvals.request(&mut f.h.state).await;
            let screen = f.draw();
            assert!(decision_queue::holds_request(&f.h.state, &id));
            let rect = f.painted_box("Decision: Permission");
            assert!(rect.y <= y && y < rect.bottom(), "{screen}");
            assert!(
                screen.contains("Allow") && screen.contains("Deny"),
                "{screen}"
            );
            assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
            if width >= 100 {
                f.acknowledged(visit);
            } else {
                f.retained(visit, presented);
            }
            clock.advance(decision_queue::INERT_AFTER_PAINT);
            let action = press(&mut f.h.state, Key::Char('n'));
            approval_bridge::handle_action(&mut f.h.state, &action, &approvals.runtime).await;
            assert_eq!(
                response.try_recv().unwrap().outcome,
                ApprovalOutcome::Reject { feedback: None }
            );
            approvals.apply_resolution(&mut f.h.state, &id);
            f.draw();
            f.acknowledged(visit);
        }
    }

    #[tokio::test]
    async fn dim_and_toast_paint_do_not_occlude_projected_rows() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut f = Visit::new(width, height).await;
            // A single clipped row under actual bottom-anchored toast paint:
            // treating toast rectangles as opaque would lose this visit.
            let y = f.pane().bottom() - 1;
            assert_eq!(f.place(y).await, vec![y]);
            let visit = f.read().await;
            notifications::notify(
                &mut f.h.state,
                NoticeSource::System,
                NoticeSeverity::Error,
                "K12-toast",
            );
            let screen = f.draw();
            assert!(screen.contains("K12-toast"), "{screen}");
            if width < 100 {
                assert!(
                    f.row(y).contains("K12-toast"),
                    "toast overlaps the only projected row"
                );
            } else {
                assert!(
                    f.row(y).ends_with('┘'),
                    "wide toast's actual bottom rail overlaps the row"
                );
            }
            f.acknowledged(visit);

            if width == 130 {
                // Help dims its whole region, including rows outside the
                // centered opaque box. Only those exterior rows are projected.
                let mut f = Visit::new(width, height).await;
                f.place(f.pane().y).await;
                press(&mut f.h.state, Key::Ctrl('x'));
                press(&mut f.h.state, Key::Char('?'));
                f.draw();
                let rect = f.painted_box("Help");
                press(&mut f.h.state, Key::Esc);
                let y = rect.bottom();
                let rows = f.place(y).await;
                assert!(rows.iter().all(|&y| y >= rect.bottom()));
                let visit = f.read().await;
                press(&mut f.h.state, Key::Ctrl('x'));
                press(&mut f.h.state, Key::Char('?'));
                f.draw();
                assert_eq!(f.painted_box("Help"), rect);
                let buffer = f.h.terminal.backend().buffer();
                assert_eq!(buffer[(f.pane().x, y)].symbol(), "│");
                assert!(buffer[(f.pane().x, y)].modifier.contains(Modifier::DIM));
                f.acknowledged(visit);
            }
        }
    }
}
