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
    ApprovalOutcome, FocusState, NotificationsConfig, PermissionResolution, RequestId,
    SkillTrustResponse, ToolRisk,
    palette::{PaletteAction, PaletteEntry, PaletteScope},
    tool_call::ApprovalSource,
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
async fn story21_6_queued_requests_preserve_the_export_frame() {
    use crate::conformance_21_6_decision_queue::raisers::{facts_6d, press as frame_press, raise};
    use crate::conformance_21_6_decision_queue::support::{paint, ready};
    let (mut state, clock) = mock_state();
    let mut approvals = Approvals::new();
    raise(
        &mut state,
        DecisionKind::ExportOverwrite,
        "deferred-export.md",
    );
    let export_frame = state.modal_stack.top().unwrap().clone();
    let (first, mut first_response) = approvals.request(&mut state).await;
    let (second, mut second_response) = approvals.request(&mut state).await;
    assert_eq!(state.modal_stack.top(), Some(&export_frame));
    assert_eq!(state.decisions.len(), 3);
    assert!(matches!(
        first_response.try_recv(),
        Err(TryRecvError::Empty)
    ));
    assert!(matches!(
        second_response.try_recv(),
        Err(TryRecvError::Empty)
    ));
    paint(&mut state, &facts_6d("c1"), 80, 24);
    ready(&mut state, &clock);
    assert_eq!(
        frame_press(&mut state, Key::Char('n')),
        InputAction::CancelExportOverwrite
    );
    assert!(matches!(
        state.decisions.take_resolved(),
        Some(DecisionSubject::ExportOverwrite(_))
    ));
    assert!(decision_queue::holds_request(&state, &first));
    assert!(decision_queue::holds_request(&state, &second));
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
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use rustain::adapters::noop::*;
    use rustain::adapters::tui::{
        decision_queue::{self, Arrival, DecisionDraft, DecisionKind},
        keymap::Host,
        notifications::{self, NoticeSeverity, NoticeSource},
        presentation::{self, Shell},
    };
    use rustain::domain::models::{
        AppConfig, Direction, JournalRecord, LogVisitCandidate, PeerId, RejectReason, RoomEvent,
        SandboxPolicy, TabManager,
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

        /// Paint through the shared compositor and model the host's draw result.
        fn compose_log(&mut self, complete: bool) -> String {
            let layout = super::super::k3_compose::layout(&self.h.state);
            let state = &self.h.state;
            self.h
                .terminal
                .draw(|frame| {
                    presentation::compose(
                        frame,
                        state,
                        &Shell::new(frame.area(), &layout),
                        Host::Local,
                        &LogFacts::default(),
                        &mut presentation::occlusion::OpaqueRects::new(layout.chat_pane),
                        |_| {},
                    );
                })
                .unwrap();
            let screen = self.h.screen_text();
            if complete {
                self.h.state.log_visits_presented();
            } else {
                self.h.state.restore_unflushed_log_visits();
            }
            screen
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
            assert_eq!(
                self.h.state.modal_stack.top().unwrap().surface,
                Surface::MissionControl
            );
            visit
        }

        fn pane(&self) -> Rect {
            super::super::k3_compose::layout(&self.h.state).chat_pane
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
                "the log was not presented; the visit must remain pending",
            );
            assert_eq!(self.h.state.log_awareness.presented.len(), presented);
        }
    }

    /// A higher Help frame prevents a visit even when the log was painted beneath it.
    pub(super) async fn fully_occluded_block_is_not_presented() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut f = Visit::new(width, height).await;
            let visit = f.read().await;
            let presented = f.h.state.log_awareness.presented.len();
            press(&mut f.h.state, Key::Ctrl('x'));
            press(&mut f.h.state, Key::Char('?'));
            let screen = f.compose_log(true);
            assert!(screen.contains("Help"), "{screen}");
            f.retained(visit, presented);
            assert_eq!(press(&mut f.h.state, Key::Esc), InputAction::Consumed);
            let screen = f.compose_log(true);
            assert!(screen.contains("Mission Control · Messages"), "{screen}");
            assert!(screen.contains("refused"), "{screen}");
            f.acknowledged(visit);
        }
    }

    #[tokio::test]
    async fn streaming_indicator_does_not_present_an_overwritten_log_row() {
        for streaming in [false, true] {
            let mut f = Visit::new(80, 24).await;
            let visit = f.read().await;
            let presented = f.h.state.log_awareness.presented.len();
            f.h.streaming.is_streaming = streaming;
            let layout = super::super::k3_compose::layout(&f.h.state);
            let area = f.h.terminal.backend().buffer().area;
            let rect = presentation::geometry::frame_rect(presentation::geometry::modal_region(
                &layout, area,
            ));
            // A Stage indicator no longer owns the log. A failed draw cannot commit it.
            let screen = f.compose_log(false);
            assert!(screen.contains("Mission Control · Messages"), "{screen}");
            let buffer = f.h.terminal.backend().buffer();
            assert_eq!(buffer[(rect.x, rect.y)].symbol(), "┌");
            assert_eq!(buffer[(rect.right() - 1, rect.bottom() - 1)].symbol(), "┘");
            f.retained(visit, presented);
            let screen = f.draw();
            assert!(screen.contains("Mission Control · Messages"), "{screen}");
            f.acknowledged(visit);
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

    /// The generic `open_quit` routes with default (no active tab) facts, which closes a
    /// conversation-scoped decision before quitting: this opener uses the active tab's facts.
    fn quit_over(state: &mut rustain::adapters::tui::state::TuiState) {
        use crate::conformance_21_6_decision_queue::raisers::press as frame_press;
        use rustain::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost};
        let _ = frame_press(state, Key::Ctrl('q'));
        let _ = quit_confirm::request(
            state,
            QuitFacts {
                streams: 1,
                ..QuitFacts::idle(QuitHost::Local)
            },
        );
    }

    fn screen_rails(screen: &str) -> Vec<String> {
        let keys = [
            "[y]", "[n]", "[d]", "[c]", "[Enter]", "[Esc]", "[e]", "[a]", "[s]", "[f]", "[r]",
            "[x]", "[i]", "[l]",
        ];
        screen
            .lines()
            .filter(|row| keys.iter().any(|key| row.contains(key)))
            // The Stage's background text beside a centred frame is not part of the rail.
            .map(|row| match (row.find('│'), row.rfind('│')) {
                (Some(left), Some(right)) if right > left => {
                    row[left + '│'.len_utf8()..right].trim().to_owned()
                }
                _ => row.to_owned(),
            })
            .collect()
    }

    fn decline_action(kind: DecisionKind) -> InputAction {
        match kind {
            DecisionKind::Consolidation => InputAction::ConsolidateDeclineAll,
            DecisionKind::Forget => InputAction::ForgetDeclineAll,
            DecisionKind::ArtifactApply | DecisionKind::ArtifactResolve => {
                InputAction::ApplyCardDecline
            }
            DecisionKind::PeerAdd => InputAction::PeerAddDecline,
            DecisionKind::TeamRetract => InputAction::TeamRetractDecline,
            DecisionKind::Fork => InputAction::ForkCancel,
            DecisionKind::Rewind => InputAction::RewindCancel,
            DecisionKind::ExportOverwrite => InputAction::CancelExportOverwrite,
            _ => panic!("not a 6-D kind: {kind:?}"),
        }
    }

    /// Every 6-D frame owns its rail while a real automatic trust request waits.
    #[tokio::test]
    async fn story21_6_mixed_frames_paint_and_decide_only_the_visible_owner() {
        use crate::conformance_21_6_decision_queue::raisers::{
            ALL_6D, PLAIN, facts_6d, press as frame_press, raise,
        };
        use crate::conformance_21_6_decision_queue::support::{paint, screen};
        for kind in ALL_6D {
            let mut solo = Visit::new(130, 40).await;
            raise(&mut solo.h.state, kind, PLAIN);
            let solo_screen = screen(&paint(&mut solo.h.state, &facts_6d("c1"), 130, 40));
            assert!(solo_screen.contains("[n]"), "{kind:?}: {solo_screen}");
            let solo_rails = screen_rails(&solo_screen);
            assert!(!solo_rails.is_empty(), "{kind:?}: {solo_screen}");
            solo.ready();
            let expected = frame_press(&mut solo.h.state, Key::Char('n'));
            assert_eq!(expected, decline_action(kind), "{kind:?}");

            let mut f = Visit::new(130, 40).await;
            raise(&mut f.h.state, kind, PLAIN);
            let frame = f.h.state.modal_stack.top().unwrap().clone();
            let mut response = trust(&mut f.h.state, "waiting-mixed-trust");
            assert_eq!(f.h.state.modal_stack.top(), Some(&frame), "{kind:?}");
            let painted = screen(&paint(&mut f.h.state, &facts_6d("c1"), 130, 40));
            assert!(painted.contains("[n]"), "{kind:?}: {painted}");
            assert_eq!(screen_rails(&painted), solo_rails, "{kind:?}");
            assert!(
                !painted.contains("waiting-mixed-trust"),
                "{kind:?}: {painted}"
            );
            f.ready();
            assert_eq!(
                frame_press(&mut f.h.state, Key::Char('n')),
                expected,
                "{kind:?}"
            );
            assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
            assert!(f.h.state.decisions.holds(DecisionKind::SkillTrust));
        }
    }

    /// Quit is now available from every migrated frame and Back restores that frame.
    #[test]
    fn story21_6_each_card_requests_quit_and_restores_its_frame() {
        use crate::conformance_21_6_decision_queue::raisers::{
            ALL_6D, PLAIN, open_ready, press as frame_press,
        };
        use rustain::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost, QuitRequest};
        for kind in ALL_6D {
            let (mut state, _clock, _hold) = open_ready(kind, PLAIN);
            let before = state.modal_stack.top().unwrap().clone();
            assert_eq!(
                frame_press(&mut state, Key::Ctrl('q')),
                InputAction::Quit,
                "{kind:?}"
            );
            assert_eq!(
                quit_confirm::request(
                    &mut state,
                    QuitFacts {
                        streams: 1,
                        ..QuitFacts::idle(QuitHost::Local)
                    }
                ),
                QuitRequest::Confirming
            );
            assert_eq!(
                frame_press(&mut state, Key::Char('n')),
                InputAction::Consumed
            );
            assert_eq!(state.modal_stack.top(), Some(&before), "{kind:?}");
            assert_eq!(
                frame_press(&mut state, Key::Char('n')),
                decline_action(kind),
                "{kind:?}"
            );
        }
    }

    /// Requested confirmations interrupt Help; the automatic consolidation waits.
    #[test]
    fn story21_6_requested_frames_restore_help_and_automatic_arrivals_wait() {
        use crate::conformance_21_6_decision_queue::raisers::{
            ALL_6D, PLAIN, facts_6d, press as frame_press, raise,
        };
        use crate::conformance_21_6_decision_queue::support::{clocked, paint, ready};
        for kind in ALL_6D {
            let (mut state, clock) = clocked(80, 24);
            reducer::open(&mut state, Surface::Help);
            let help = state.modal_stack.top().unwrap().clone();
            // Recent typing prevents an automatic arrival from interrupting Help.
            state.last_printable_at = Some(state.notifications.now());
            raise(&mut state, kind, PLAIN);
            if kind == DecisionKind::Consolidation {
                assert_eq!(state.modal_stack.top(), Some(&help));
                assert!(decision_queue::consolidation_pending(&state, "c1"));
                continue;
            }
            assert_eq!(
                state.modal_stack.top().unwrap().surface,
                Surface::Decision,
                "{kind:?}"
            );
            assert_eq!(state.modal_stack.len(), 2, "{kind:?}");
            paint(&mut state, &facts_6d("c1"), 80, 24);
            ready(&mut state, &clock);
            assert_eq!(
                frame_press(&mut state, Key::Char('n')),
                decline_action(kind),
                "{kind:?}"
            );
            assert_eq!(state.modal_stack.top(), Some(&help), "{kind:?}");
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

    /// All 6-D owners retain their complete solo rail against every automatic 6-C
    /// arrival, including a second rail whose labels do not name the waiting item.
    #[tokio::test]
    async fn six_d_owners_paint_only_their_solo_rail_for_automatic_arrivals() {
        use crate::conformance_21_6_decision_queue::raisers::{
            ALL_6D, PLAIN, facts_6d, press as frame_press, raise,
        };
        use crate::conformance_21_6_decision_queue::support::{paint, screen};
        let automatic = [
            DecisionKind::Question,
            DecisionKind::SkillTrust,
            DecisionKind::PlanApproval,
            DecisionKind::PlanCard,
            DecisionKind::PlanDeviation,
            DecisionKind::Delegation,
        ];
        let mut reached = 0;
        for first in ALL_6D {
            let mut solo = Visit::new(130, 40).await;
            let _solo_hold = raise(&mut solo.h.state, first, PLAIN);
            let solo_screen = screen(&paint(&mut solo.h.state, &facts_6d("c1"), 130, 40));
            let solo_rails = screen_rails(&solo_screen);
            assert!(!solo_rails.is_empty(), "{first:?}: {solo_screen}");
            solo.ready();
            let solo_action = frame_press(&mut solo.h.state, Key::Char('n'));
            assert_eq!(solo_action, decline_action(first), "{first:?}");
            for second in automatic {
                let mut f = Visit::new(130, 40).await;
                let _first_hold = raise(&mut f.h.state, first, PLAIN);
                let _second_hold = raise(&mut f.h.state, second, PLAIN);
                let at = format!("{first:?} -> {second:?}");
                assert_eq!(f.h.state.decisions.undecided(), 2, "{at}");
                assert_eq!(
                    f.h.state
                        .modal_stack
                        .iter()
                        .filter(|frame| frame.surface == Surface::Decision)
                        .count(),
                    1,
                    "{at}"
                );
                let painted = screen(&paint(&mut f.h.state, &facts_6d("c1"), 130, 40));
                assert_eq!(screen_rails(&painted), solo_rails, "{at}");
                f.ready();
                assert_eq!(
                    frame_press(&mut f.h.state, Key::Char('n')),
                    solo_action,
                    "{at}"
                );
                assert!(f.h.state.decisions.holds(second), "{at}");
                reached += 1;
            }
        }
        assert_eq!(reached, 54);
    }

    /// Requested cards interrupt retained Help/NotificationLog callers. Quit
    /// covers every owner instead; Back restores the same owner and its sole rail.
    #[tokio::test]
    async fn six_d_rail_exclusivity_survives_retained_doors_and_quit_occlusion() {
        use crate::conformance_21_6_decision_queue::raisers::{
            ALL_6D, PLAIN, facts_6d, press as frame_press, raise,
        };
        use crate::conformance_21_6_decision_queue::support::{paint, screen};
        for kind in ALL_6D {
            let mut solo = Visit::new(130, 40).await;
            raise(&mut solo.h.state, kind, PLAIN);
            let solo_screen = screen(&paint(&mut solo.h.state, &facts_6d("c1"), 130, 40));
            let expected = screen_rails(&solo_screen);
            assert!(!expected.is_empty(), "{kind:?}");

            for door in [Surface::Help, Surface::NotificationLog] {
                if kind == DecisionKind::Consolidation {
                    continue; // Automatic arrival waits beneath these callers.
                }
                let mut f = Visit::new(130, 40).await;
                reducer::open(&mut f.h.state, door);
                let caller = f.h.state.modal_stack.top().unwrap().clone();
                raise(&mut f.h.state, kind, PLAIN);
                assert_eq!(
                    f.h.state.modal_stack.top().unwrap().surface,
                    Surface::Decision
                );
                let _waiting = trust(&mut f.h.state, "waiting-door-trust");
                let painted = screen(&paint(&mut f.h.state, &facts_6d("c1"), 130, 40));
                assert_eq!(screen_rails(&painted), expected, "{kind:?} over {door:?}");
                f.ready();
                assert_eq!(
                    frame_press(&mut f.h.state, Key::Char('n')),
                    decline_action(kind)
                );
                assert_eq!(
                    f.h.state.modal_stack.top(),
                    Some(&caller),
                    "{kind:?} over {door:?}"
                );
                assert!(f.h.state.decisions.holds(DecisionKind::SkillTrust));
            }

            let mut f = Visit::new(130, 40).await;
            raise(&mut f.h.state, kind, PLAIN);
            let owner = f.h.state.modal_stack.top().unwrap().clone();
            quit_over(&mut f.h.state);
            let mut solo_quit = Visit::new(130, 40).await;
            raise(&mut solo_quit.h.state, kind, PLAIN);
            quit_over(&mut solo_quit.h.state);
            let solo_covered = screen(&paint(&mut solo_quit.h.state, &facts_6d("c1"), 130, 40));
            let _waiting = trust(&mut f.h.state, "waiting-quit-trust");
            let covered = screen(&paint(&mut f.h.state, &facts_6d("c1"), 130, 40));
            assert_eq!(
                screen_rails(&covered),
                screen_rails(&solo_covered),
                "{kind:?} under Quit"
            );
            // The quit frame's title names the owner beneath it (`Decision: X › System · Quit`);
            // the owner's own body (its breadcrumb row) is covered.
            assert!(
                !covered.contains(&format!("Needs you › {}", kind.title())),
                "{kind:?}: {covered}"
            );
            assert_eq!(
                frame_press(&mut f.h.state, Key::Char('n')),
                InputAction::Consumed
            );
            assert_eq!(f.h.state.modal_stack.top(), Some(&owner), "{kind:?}");
            let restored = screen(&paint(&mut f.h.state, &facts_6d("c1"), 130, 40));
            assert_eq!(screen_rails(&restored), expected, "{kind:?} after Quit");
        }
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
        use rustain::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost, QuitRequest};
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
            f.h.state.modal_stack.top().unwrap().surface,
            Surface::Decision
        );
        assert!(decision_queue::artifact_pending(
            &f.h.state,
            &conversation_id
        ));
        assert!(f.draw().contains("[y] Apply"));
        let apply_frame = f.h.state.modal_stack.top().unwrap().clone();
        // The generic open_quit helper routes with default (no active tab) facts,
        // which would close this conversation-scoped decision before requesting quit.
        assert_eq!(
            frame_press(&mut f.h.state, Key::Ctrl('q')),
            InputAction::Quit
        );
        assert_eq!(
            quit_confirm::request(
                &mut f.h.state,
                QuitFacts {
                    streams: 1,
                    ..QuitFacts::idle(QuitHost::Local)
                }
            ),
            QuitRequest::Confirming
        );
        assert_eq!(
            f.h.state.modal_stack.top().unwrap().surface,
            Surface::QuitConfirm
        );
        // The trust prompt and a permission arrive under the quit frame: both queue.
        let mut response = trust(&mut f.h.state, "deferred-real-trust");
        let mut approvals = Approvals::new();
        let (_, mut permission_response) = approvals.request(&mut f.h.state).await;
        assert_eq!(f.h.state.decisions.len(), 3);
        assert_eq!(
            frame_press(&mut f.h.state, Key::Char('n')),
            InputAction::Consumed
        );
        assert_eq!(f.h.state.modal_stack.top(), Some(&apply_frame));
        let screen = f.draw();
        assert!(screen.contains("[y] Apply"), "{screen}");
        assert!(!screen.contains("deferred-real-trust"), "{screen}");
        assert!(!f._workspace.path().join("visible.txt").exists());
        f.ready();
        assert_eq!(
            frame_press(&mut f.h.state, Key::Char('y')),
            InputAction::ApplyCardAccept
        );
        let Some(DecisionSubject::Artifact(card)) = f.h.state.decisions.take_resolved() else {
            panic!("the accepted frame returns its artifact payload");
        };
        artifact_bridge::apply_confirmed_card(&mut f.h.state, &f.app, *card, PermissionMode::Yolo)
            .await;
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
            f.h.state.decisions.take_resolved().is_none(),
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
        assert!(!decision_queue::artifact_pending(
            &f.h.state,
            &conversation_id
        ));
    }

    #[tokio::test]
    async fn one_uncovered_projected_row_acknowledges_despite_inline_search() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut f = Visit::new(width, height).await;
            let visit = f.read().await;
            let presented = f.h.state.log_awareness.presented.len();
            let initial = f.compose_log(false);
            assert!(initial.contains("Search"), "{initial}");
            f.retained(visit, presented);
            // Search moved from the Stage's Ctrl+F frame to the log's own `/` field.
            assert_eq!(press(&mut f.h.state, Key::Char('/')), InputAction::Consumed);
            assert!(
                f.h.state
                    .mission_control
                    .messages
                    .as_ref()
                    .unwrap()
                    .search
                    .editing
            );
            assert_eq!(
                f.h.state.modal_stack.top().unwrap().surface,
                Surface::MissionControl
            );
            let screen = f.compose_log(true);
            assert!(screen.contains("Mission Control · Messages"), "{screen}");
            assert!(screen.contains("Keep filter"), "{screen}");
            f.acknowledged(visit);
            assert_eq!(press(&mut f.h.state, Key::Esc), InputAction::Consumed);
            assert!(
                !f.h.state
                    .mission_control
                    .messages
                    .as_ref()
                    .unwrap()
                    .search
                    .editing
            );
        }
    }

    #[tokio::test]
    async fn absent_and_not_painted_log_blocks_do_not_acknowledge() {
        for too_small in [false, true] {
            let mut f = Visit::new(80, 24).await;
            let visit = f.read().await;
            let presented = f.h.state.log_awareness.presented.len();
            if too_small {
                f.h.terminal = Terminal::new(TestBackend::new(59, 15)).unwrap();
                let screen = f.draw();
                assert!(screen.contains("Terminal too small"), "{screen}");
            } else {
                reducer::close(&mut f.h.state, Surface::MissionControl);
                let screen = f.compose_log(true);
                assert!(!screen.contains("Mission Control · Messages"), "{screen}");
            }
            f.retained(visit, presented);
            if too_small {
                f.h.terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            } else {
                assert_eq!(f.read().await, visit);
            }
            let screen = f.compose_log(true);
            assert!(screen.contains("Mission Control · Messages"), "{screen}");
            f.acknowledged(visit);
        }
    }

    #[tokio::test]
    async fn which_key_and_decision_frame_cover_the_actual_clipped_row() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            for decision in [false, true] {
                let mut f = Visit::new(width, height).await;
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
                let screen = f.compose_log(true);
                assert!(
                    screen.contains(if decision { "Daily budget" } else { "Ctrl+X" }),
                    "{screen}"
                );
                f.retained(visit, presented);
                if decision {
                    f.ready();
                }
                assert_eq!(press(&mut f.h.state, Key::Esc), InputAction::Consumed);
                assert_eq!(
                    f.h.state.modal_stack.top().unwrap().surface,
                    Surface::MissionControl
                );
                assert!(f.h.state.pending_leader.is_none());
                let screen = f.compose_log(true);
                assert!(screen.contains("Mission Control · Messages"), "{screen}");
                f.acknowledged(visit);
            }
        }
    }

    #[tokio::test]
    async fn permission_frame_covers_the_actual_clipped_row() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut f = Visit::new(width, height).await;
            let visit = f.read().await;
            let presented = f.h.state.log_awareness.presented.len();
            let mut approvals = Approvals::new();
            let (id, mut response) = approvals.request(&mut f.h.state).await;
            let item = f.h.state.decisions.items().next().unwrap();
            let key = decision_queue::DecisionKey {
                kind: item.kind,
                source: item.source.clone(),
                conversation_id: item.conversation_id.clone(),
            };
            assert!(decision_queue::open(&mut f.h.state, &key));
            let screen = f.compose_log(true);
            assert!(decision_queue::holds_request(&f.h.state, &id));
            assert!(
                screen.contains("Allow") && screen.contains("Deny"),
                "{screen}"
            );
            assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
            f.retained(visit, presented);
            f.ready();
            let action = press(&mut f.h.state, Key::Char('n'));
            approval_bridge::handle_action(&mut f.h.state, &action, &approvals.runtime).await;
            assert_eq!(
                response.try_recv().unwrap().outcome,
                ApprovalOutcome::Reject { feedback: None }
            );
            approvals.apply_resolution(&mut f.h.state, &id);
            let screen = f.compose_log(true);
            assert!(screen.contains("Mission Control · Messages"), "{screen}");
            f.acknowledged(visit);
        }
    }

    #[tokio::test]
    async fn dim_and_toast_paint_do_not_occlude_projected_rows() {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut f = Visit::new(width, height).await;
            let visit = f.read().await;
            notifications::notify(
                &mut f.h.state,
                NoticeSource::System,
                NoticeSeverity::Error,
                "K12-toast",
            );
            let layout = super::super::k3_compose::layout(&f.h.state);
            let area = f.h.terminal.backend().buffer().area;
            let rect = presentation::geometry::frame_rect(presentation::geometry::modal_region(
                &layout, area,
            ));
            // Paint the same live toast separately to locate actual cells before
            // compose puts the Log frame over the toast layer.
            let mut toast_terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            toast_terminal
                .draw(|frame| {
                    rustain::adapters::tui::widgets::toast_layer::render(
                        frame,
                        layout.chat_pane,
                        &f.h.state.notifications,
                        &f.h.state.theme,
                        &LogFacts::default(),
                    );
                })
                .unwrap();
            let toast = toast_terminal.backend().buffer();
            let toast_screen = (0..height)
                .map(|y| super::super::k3_compose::row(toast, area, y))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(toast_screen.contains("K12-toast"), "{toast_screen}");
            let overlap: Vec<_> = (rect.y..rect.bottom())
                .flat_map(|y| {
                    (rect.x..rect.right())
                        .filter_map(move |x| (toast[(x, y)].symbol() != " ").then_some((x, y)))
                })
                .collect();
            assert!(
                !overlap.is_empty(),
                "painted toast cells intersect the log frame: {toast_screen}"
            );
            assert!(f.h.state.pending_leader.is_none());
            assert_eq!(
                f.h.state.modal_stack.top().unwrap().surface,
                Surface::MissionControl
            );
            let screen = f.compose_log(true);
            let composed = f.h.terminal.backend().buffer();
            assert!(
                overlap
                    .iter()
                    .any(|&(x, y)| composed[(x, y)].symbol() != toast[(x, y)].symbol()),
                "the frame actually covers toast cells: {screen}"
            );
            if width >= 100 {
                use ratatui::style::Modifier;
                assert!(
                    (layout.chat_pane.y..layout.chat_pane.bottom()).any(|y| {
                        (layout.chat_pane.x..layout.chat_pane.right()).any(|x| {
                            (x < rect.x || x >= rect.right() || y < rect.y || y >= rect.bottom())
                                && toast[(x, y)].symbol() != " "
                                && composed[(x, y)].symbol() == toast[(x, y)].symbol()
                                && composed[(x, y)].modifier.contains(Modifier::DIM)
                        })
                    }),
                    "the toast outside the centered frame is genuinely dimmed: {screen}"
                );
            }
            assert!(screen.contains("Mission Control · Messages"), "{screen}");
            f.acknowledged(visit);
        }
    }
}
