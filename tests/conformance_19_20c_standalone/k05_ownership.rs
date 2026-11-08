use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{
    self, Arrival, DecisionDraft, DecisionKind, DecisionSubject, PaneOutcome, handle_domain_event,
    note_painted, raise, raise_large_image, withdraw,
};
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::notifications::NotificationStore;
use rustain::adapters::tui::presentation::{Surface, reducer};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::clock::MockClock;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::NotificationsConfig;
use rustain::domain::models::{FocusState, ImageAttachment};
use rustain::domain::services::approval_runtime::{
    ApprovalRuntimeEvent, CancelReason, ReviewScope,
};
use rustain::infrastructure::runtime::approval_bridge;
use serde_json::json;

use super::harness::{Fixture, VIEWPORTS, request_id, text};

#[allow(clippy::duplicate_mod)]
#[path = "../common/render_helpers.rs"]
mod stage;

#[tokio::test]
async fn k05_review_decision_targets_pinned_request_not_queue() {
    let mut f = Fixture::new(64).await;
    let first =
        json!({"file_path": "a.txt", "content": format!("{}\nPINNED-A-TAIL", "a".repeat(500))});
    let second = json!({"file_path": "b.txt", "content": "QUEUED-B-TAIL"});
    f.submit(vec![("Write", first.clone())]).await;
    f.wait_requested().await;
    let a = f.current_id();
    f.paint(60, 16);
    f.key(KeyCode::Char('r')).await;
    f.state.input_buffer = "untouched composer draft".into();
    f.state.cursor_position = f.state.input_buffer.chars().count();
    f.submit(vec![("Write", second)]).await;
    let (event, _) = f.wait_requested().await;
    let b = request_id(&event);
    assert_ne!(a, b);
    assert_eq!(
        f.current_id(),
        a,
        "arrival cannot replace the reviewed slot"
    );
    assert!(f.permission().reviewing);
    assert_eq!(f.state.decisions.count(DecisionKind::Permission), 2);
    for (width, height) in VIEWPORTS {
        let painted = text(&f.review_pages(width, height).await);
        assert!(painted.contains("PINNED-A-TAIL"));
        assert!(!painted.contains("QUEUED-B-TAIL"));
    }
    assert!(matches!(
        f.key(KeyCode::Char('y')).await,
        Some(InputAction::PermissionAllow)
    ));
    let (resolved, _) = f.next_approval().await;
    assert!(
        matches!(resolved, ApprovalRuntimeEvent::Resolved { id, .. } if id == a),
        "Review decision must resolve A, not queued B"
    );
    // Replaying the effect cannot consume or answer the promoted request.
    approval_bridge::handle_action(&mut f.state, &InputAction::PermissionAllow, &f.runtime).await;
    assert_eq!(f.current_id(), b);
    assert!(
        f.runtime
            .pending_review(&b, ReviewScope::AllTools)
            .await
            .is_some()
    );
    assert_eq!(f.state.input_buffer, "untouched composer draft");
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&b).await;
    f.finish().await;
    assert_eq!(*f.tools.executed.lock(), vec![("Write".into(), first)]);
}

#[tokio::test]
async fn k05_review_feedback_escape_and_queue_keep_request_ownership() {
    let mut f = Fixture::new(64).await;
    f.submit(vec![(
        "Write",
        json!({"file_path": "a", "content": "REVIEW-A".repeat(60)}),
    )])
    .await;
    f.wait_requested().await;
    let a = f.current_id();
    f.paint(60, 16);
    f.key(KeyCode::Char('r')).await;
    f.submit(vec![(
        "Edit",
        json!({"file_path": "b", "old_string": "x", "new_string": "QUEUED-B"}),
    )])
    .await;
    let (event, _) = f.wait_requested().await;
    let b = request_id(&event);
    assert_eq!(f.current_id(), a);
    assert!(f.permission().reviewing);
    assert!(matches!(
        f.key(KeyCode::Esc).await,
        Some(InputAction::Consumed)
    ));
    assert!(!f.permission().reviewing);
    assert_eq!(f.current_id(), a, "Review Esc is undecided");
    assert!(f.tools.executed.lock().is_empty());
    assert!(
        f.runtime
            .pending_review(&a, ReviewScope::AllTools)
            .await
            .is_some()
    );
    f.key(KeyCode::Char('r')).await;
    f.key(KeyCode::Char('f')).await;
    assert_eq!(f.current_id(), a);
    assert!(f.permission().feedback.is_some());
    for c in "feedback draft".chars() {
        f.key(KeyCode::Char(c)).await;
    }
    f.submit(vec![("Bash", json!({"command": "printf QUEUED-C"}))])
        .await;
    let (event, _) = f.wait_requested().await;
    let c = request_id(&event);
    assert_eq!(f.current_id(), a);
    assert_eq!(
        f.permission().feedback.as_ref().unwrap().as_str(),
        "feedback draft"
    );
    assert_eq!(f.state.decisions.count(DecisionKind::Permission), 3);
    let feedback_paint = f.paint(60, 16).join("\n");
    assert!(feedback_paint.contains("Feedback:"), "{feedback_paint}");
    assert!(
        feedback_paint.contains("feedback draft"),
        "{feedback_paint}"
    );
    assert!(matches!(
        f.key(KeyCode::Esc).await,
        Some(InputAction::Consumed)
    ));
    assert_eq!(f.current_id(), a);
    assert!(
        f.permission().reviewing,
        "feedback Esc returns to the pinned Review"
    );
    let restored_paint = f.paint(60, 16).join("\n");
    assert!(
        restored_paint.contains("Decision: Permission"),
        "{restored_paint}"
    );
    f.key(KeyCode::Esc).await;
    assert!(!f.permission().reviewing);
    assert_eq!(f.current_id(), a);
    // Esc defers A; it cannot promote B or answer any request.
    assert!(matches!(
        f.key(KeyCode::Esc).await,
        Some(InputAction::Consumed)
    ));
    assert!(f.state.modal_stack.is_empty());
    assert!(f.state.decisions.items().next().unwrap().deferred);
    assert!(f.tools.executed.lock().is_empty());
    decision_queue::open_waiting(&mut f.state, &LogFacts::default());
    f.key(KeyCode::Enter).await;
    // The queue selects the oldest non-deferred request.
    assert_eq!(f.current_id(), b);
    assert!(matches!(
        f.key(KeyCode::Char('n')).await,
        Some(InputAction::PermissionDeny)
    ));
    f.wait_closed(&b).await;
    assert_eq!(f.current_id(), c);
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&c).await;
    decision_queue::open_waiting(&mut f.state, &LogFacts::default());
    f.key(KeyCode::Enter).await;
    assert_eq!(f.current_id(), a);
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&a).await;
    f.finish().await;
    assert!(f.tools.executed.lock().is_empty());
}

/// The event loop's pane dispatch, as the loop calls it: `Some` when the pane owned the key.
fn pane_key(state: &mut TuiState, key: char) -> Option<InputAction> {
    match handle_domain_event(
        state,
        &DomainInputEvent::KeyPress(key),
        &LogFacts::default(),
        Host::Local,
    )? {
        PaneOutcome::Effect(effect) => Some(effect.into_action()),
        _ => Some(InputAction::Consumed),
    }
}

#[tokio::test]
async fn k05_higher_image_confirmation_and_raw_key_guards_do_not_decide() {
    let mut f = Fixture::new(64).await;
    let input = json!({"command": "printf POSITIVE-CANARY"});
    f.submit(vec![("Bash", input.clone())]).await;
    f.wait_requested().await;
    let id = f.current_id();
    f.paint(60, 16);
    f.key(KeyCode::Char('r')).await;
    // Old -> new (21.6 6-A): the TuiState large-image slot is gone; the queued image
    // item (raised below) is the higher-priority confirmation and carries the image.
    // Old (19.20c): `handle_input` mapped Chat-focus `y` to ImageConfirmAttach itself.
    // New (21.1 S2, AC9/AC12): the confirmation is a decision-pane item that owns its
    // keys before approval does (the event loop's pane dispatch runs first), inert for
    // its first 500 ms; `handle_input` no longer maps the key.
    // The requested image temporarily takes the decision frame.
    raise_large_image(
        &mut f.state,
        ImageAttachment {
            media_type: "image/png".into(),
            data: "confirmation-fixture".into(),
        },
        "Large image. Attach it?",
    );
    note_painted(&mut f.state);
    assert!(matches!(
        pane_key(&mut f.state, 'y'),
        Some(InputAction::Consumed)
    ));
    f.paint(60, 16);
    assert!(matches!(
        pane_key(&mut f.state, 'y'),
        Some(InputAction::ImageConfirmAttach)
    ));
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    decision_queue::open_waiting(&mut f.state, &LogFacts::default());
    f.key(KeyCode::Enter).await;
    assert_eq!(f.current_id(), id);
    f.key(KeyCode::Char('r')).await;
    assert!(f.permission().reviewing);
    assert!(
        f.runtime
            .pending_review(&id, ReviewScope::AllTools)
            .await
            .is_some()
    );
    assert!(f.tools.executed.lock().is_empty());
    // Image attachment effects belong to the image dispatcher, not approval; the
    // answered item left the queue (old: the fixture slot was cleared by hand to model
    // its completed higher-priority confirmation).
    assert!(!f.state.decisions.holds(DecisionKind::LargeImage));
    for key in ['y', 'n', 'a', 's', 'f', 'r'] {
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SHIFT,
        ] {
            let action = f
                .raw(Event::Key(KeyEvent::new(KeyCode::Char(key), modifiers)))
                .await;
            assert!(
                !matches!(
                    action,
                    Some(
                        InputAction::PermissionAllow
                            | InputAction::PermissionDeny
                            | InputAction::PermissionAlwaysAllow
                            | InputAction::PermissionSessionAllow
                            | InputAction::FeedbackInputSubmit
                    )
                ),
                "modified {key} must not decide: {modifiers:?}"
            );
            assert_eq!(f.current_id(), id);
            assert!(f.permission().feedback.is_none());
        }
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(
                f.raw(Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char(key),
                    KeyModifiers::NONE,
                    kind
                )))
                .await
                .is_none()
            );
            assert_eq!(f.current_id(), id);
        }
    }
    assert!(f.tools.executed.lock().is_empty());
    assert!(f.permission().reviewing);
    assert!(matches!(
        f.raw(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )))
        .await,
        Some(InputAction::Consumed)
    ));
    assert!(matches!(
        f.raw(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL
        )))
        .await,
        Some(InputAction::Quit)
    ));
    assert!(
        f.runtime
            .pending_review(&id, ReviewScope::AllTools)
            .await
            .is_some()
    );
    f.key(KeyCode::Char('y')).await;
    f.wait_closed(&id).await;
    f.finish().await;
    assert_eq!(*f.tools.executed.lock(), vec![("Bash".into(), input)]);
}

/// The second real UI consumer settles the very same genuine bridge event;
/// this is not a fixture calling resolve or fabricating a pending record.
#[tokio::test]
async fn k05_resolved_request_retires_open_review_before_next_decision() {
    let mut f = Fixture::new(64).await;
    let input_a = json!({"command": "printf A-EXECUTES"});
    f.submit(vec![("Bash", input_a.clone())]).await;
    let (event_a, snapshot_a) = f.wait_requested().await;
    let a = f.current_id();
    let mut other_consumer = TuiState::new(80, 24);
    let clock = Arc::new(MockClock::new(Instant::now()));
    other_consumer.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    approval_bridge::apply(&mut other_consumer, event_a, snapshot_a);
    note_painted(&mut other_consumer);
    clock.advance(Duration::from_millis(501));
    f.paint(80, 24);
    f.key(KeyCode::Char('r')).await;
    f.submit(vec![(
        "Write",
        json!({"file_path": "b", "content": "B-MUST-NOT-EXECUTE"}),
    )])
    .await;
    let (event_b, _) = f.wait_requested().await;
    let b = request_id(&event_b);
    let decision = pane_key(&mut other_consumer, 'y').expect("real decision dispatch");
    assert!(matches!(decision, InputAction::PermissionAllow));
    approval_bridge::handle_action(&mut other_consumer, &decision, &f.runtime).await;
    let (resolved, _) = f.next_approval().await;
    assert!(matches!(resolved, ApprovalRuntimeEvent::Resolved { id, .. } if id == a));
    assert!(
        decision_queue::current(&f.state, &LogFacts::default())
            .unwrap()
            .is_stale()
    );
    assert_eq!(f.current_id(), a);
    assert!(matches!(
        f.key(KeyCode::Char('n')).await,
        Some(InputAction::Consumed)
    ));
    assert!(
        f.runtime
            .pending_review(&b, ReviewScope::AllTools)
            .await
            .is_some()
    );
    f.key(KeyCode::Esc).await;
    assert!(
        f.state.modal_stack.is_empty(),
        "closing stale A must not open B"
    );
    decision_queue::open_waiting(&mut f.state, &LogFacts::default());
    f.key(KeyCode::Enter).await;
    assert_eq!(f.current_id(), b);
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&b).await;
    f.finish().await;
    assert_eq!(*f.tools.executed.lock(), vec![("Bash".into(), input_a)]);
}

/// Current-thread scheduling is intentional: a parallel scheduler batch fills
/// the capacity-one approval broadcast before the bridge gets its next poll.
/// The final genuine Requested is a deterministic fence available even to the
/// no-rescan mutant; the assertion fails immediately, not by a missing-event timeout.
#[tokio::test(flavor = "current_thread")]
async fn k05_standalone_broadcast_lag_rescans_every_real_pending_request() {
    const REQUESTS: usize = 40;
    let mut f = Fixture::new(1).await;
    f.submit(
        (0..REQUESTS)
            .map(|n| {
                (
                    "Write",
                    json!({"file_path": format!("lag-{n}"), "content": format!("LAG-TAIL-{n}")}),
                )
            })
            .collect(),
    )
    .await;
    let pending = f.await_batch_pending(REQUESTS).await;
    let last = pending.last().expect("nonempty batch");
    loop {
        let (event, _) = f.wait_requested().await;
        if &request_id(&event) == last {
            break;
        }
    }
    assert_eq!(
        f.requested.len(),
        REQUESTS,
        "lag recovery must reissue all real pending requests before the final request"
    );
    assert_eq!(f.state.decisions.count(DecisionKind::Permission), REQUESTS);
    assert_eq!(
        f.current_id(),
        pending[0],
        "lag rescan preserves request order"
    );
    f.paint(60, 16);
    f.key(KeyCode::Char('r')).await;
    assert!(text(&f.review_pages(60, 16).await).contains("LAG-TAIL-0"));
    for expected in pending {
        assert_eq!(f.current_id(), expected);
        f.key(KeyCode::Char('n')).await;
        f.wait_closed(&expected).await;
    }
    f.finish().await;
    assert_eq!(f.closed.len(), REQUESTS);
    assert!(f.tools.executed.lock().is_empty());
    assert!(f.state.decisions.is_empty());
    assert!(f.state.modal_stack.is_empty());
}

#[tokio::test]
async fn k05_higher_plan_confirmation_keeps_arrival_queued() {
    use rustain::adapters::tui::state::PendingPlanApproval;
    use rustain::domain::models::OverlayType;
    use rustain::domain::models::visual::ConfirmationType;

    let mut f = Fixture::new(64).await;
    f.state.pending_plan_approval = Some(PendingPlanApproval {
        conversation_id: "higher-plan".into(),
        plan_path: "plan.md".into(),
        contents: "An unrelated higher confirmation".into(),
        summary: "Plan approval fixture".into(),
    });
    let plan_focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::PlanApproval));
    f.state.focus = plan_focus.clone();
    let input = json!({"command": "printf AFTER-PLAN-CANARY"});
    f.submit(vec![("Bash", input.clone())]).await;
    let (event, _) = f.wait_requested().await;
    let id = request_id(&event);
    assert_eq!(f.state.focus, plan_focus);
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    assert_eq!(f.state.decisions.count(DecisionKind::Permission), 1);
    assert!(decision_queue::status_cue(decision_queue::waiting(&f.state)).is_some());
    assert!(f.state.notifications.visible().count() > 0);
    assert!(
        f.paint(60, 16)
            .iter()
            .all(|row| !row.contains("AFTER-PLAN-CANARY"))
    );
    assert!(matches!(
        f.key(KeyCode::Char('y')).await,
        Some(InputAction::PlanApproveNormal)
    ));
    assert!(
        f.runtime
            .pending_review(&id, ReviewScope::AllTools)
            .await
            .is_some()
    );
    assert!(f.tools.executed.lock().is_empty());
    // Dismissing the unrelated plan must not open a queued decision by surprise.
    f.state.pending_plan_approval = None;
    f.state.focus = FocusState::Input;
    decision_queue::settle(&mut f.state);
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    decision_queue::open_waiting(&mut f.state, &LogFacts::default());
    f.key(KeyCode::Enter).await;
    assert_eq!(f.current_id(), id);
    f.key(KeyCode::Char('r')).await;
    assert!(text(&f.review_pages(60, 16).await).contains("AFTER-PLAN-CANARY"));
    f.key(KeyCode::Char('y')).await;
    f.wait_closed(&id).await;
    f.finish().await;
    assert_eq!(*f.tools.executed.lock(), vec![("Bash".into(), input)]);
}

#[tokio::test]
async fn k05_review_rewraps_same_request_on_resize() {
    let input = json!({
        "file_path": "resize.txt",
        "content": format!("{}\nRESIZED-EXACT-TAIL", "界 e\u{301} input ".repeat(100)),
    });
    let mut f = Fixture::new(64).await;
    f.submit(vec![("Write", input.clone())]).await;
    f.wait_requested().await;
    let id = f.current_id();
    f.key(KeyCode::Char('r')).await;
    for (width, height) in VIEWPORTS {
        f.raw(Event::Resize(width, height)).await;
        f.key(KeyCode::Char('r')).await;
        let pages = f.review_pages(width, height).await;
        let painted = text(&pages);
        assert!(
            painted.contains("RESIZED-EXACT-TAIL"),
            "{width}×{height}: {painted}"
        );
        assert_eq!(f.current_id(), id);
        assert!(f.permission().reviewing);
        assert!(f.tools.executed.lock().is_empty());
    }
    f.key(KeyCode::Char('y')).await;
    f.wait_closed(&id).await;
    f.finish().await;
    assert_eq!(*f.tools.executed.lock(), vec![("Write".into(), input)]);
}

#[tokio::test]
async fn k05_passive_overlay_dismissals_surface_deferred_approval() {
    use rustain::adapters::tui::keymap::Context;

    for (overlay, dismiss) in [
        ("help", KeyCode::Esc),
        ("help", KeyCode::Char('?')),
        ("which-key", KeyCode::Esc),
        ("which-key", KeyCode::Char('!')),
        ("palette", KeyCode::Esc),
    ] {
        let mut f = Fixture::new(64).await;
        f.state.focus = if overlay == "which-key" {
            FocusState::Chat
        } else {
            FocusState::Input
        };
        f.state.input_buffer = "composer draft".into();
        match overlay {
            "help" => {
                reducer::open(&mut f.state, Surface::Help);
            }
            "which-key" => {
                f.state.pending_leader = Some(Context::Chat);
            }
            _ => {
                reducer::open(&mut f.state, Surface::CommandPalette);
            }
        }
        let input = json!({"command": "printf AFTER-PASSIVE-OVERLAY"});
        f.submit(vec![("Bash", input.clone())]).await;
        let (event, _) = f.wait_requested().await;
        let id = request_id(&event);
        assert_eq!(f.state.decisions.count(DecisionKind::Permission), 1);
        assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
        assert!(decision_queue::status_cue(decision_queue::waiting(&f.state)).is_some());
        assert!(f.state.notifications.visible().count() > 0);
        assert!(matches!(f.key(dismiss).await, Some(InputAction::Consumed)));
        assert!(
            decision_queue::current(&f.state, &LogFacts::default()).is_none(),
            "{overlay} must not open it"
        );
        decision_queue::open_waiting(&mut f.state, &LogFacts::default());
        f.key(KeyCode::Enter).await;
        assert_eq!(f.current_id(), id);
        f.key(KeyCode::Char('r')).await;
        assert!(text(&f.review_pages(60, 16).await).contains("AFTER-PASSIVE-OVERLAY"));
        assert_eq!(f.state.input_buffer, "composer draft");
        f.key(KeyCode::Char('y')).await;
        f.wait_closed(&id).await;
        f.finish().await;
        assert_eq!(*f.tools.executed.lock(), vec![("Bash".into(), input)]);
    }
}

#[tokio::test]
async fn k05_passive_dismissal_preserves_remaining_modal_and_higher_owner() {
    use rustain::adapters::tui::keymap::Context;

    let mut f = Fixture::new(64).await;
    // The routed leader is the higher owner the permission waits behind.
    f.state.focus = FocusState::Chat;
    f.state.pending_leader = Some(Context::Chat);
    f.submit(vec![(
        "Bash",
        json!({"command": "printf AFTER-HIGHER-OWNER"}),
    )])
    .await;
    let (event, _) = f.wait_requested().await;
    let id = request_id(&event);
    assert_eq!(f.state.pending_leader, Some(Context::Chat));
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    assert_eq!(f.state.decisions.count(DecisionKind::Permission), 1);
    // Old -> new (21.6 6-A): a queued, not-deferred image item replaces the slot as the
    // higher confirmation (Automatic + a pending leader: it waits, never opens a frame).
    assert_eq!(
        raise(
            &mut f.state,
            DecisionDraft::new(DecisionKind::LargeImage, "higher-image-confirmation").subject(
                DecisionSubject::LargeImage(ImageAttachment {
                    media_type: "image/png".into(),
                    data: "higher-image-confirmation".into(),
                })
            ),
            Arrival::Automatic,
        ),
        rustain::adapters::tui::decision_queue::Raised::Waiting
    );
    f.key(KeyCode::Esc).await;
    assert_eq!(f.state.focus, FocusState::Chat);
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    assert_eq!(f.state.decisions.count(DecisionKind::Permission), 1);
    assert!(f.tools.executed.lock().is_empty());
    withdraw(&mut f.state, DecisionKind::LargeImage);
    reducer::open(&mut f.state, Surface::Help);
    f.key(KeyCode::Esc).await;
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    decision_queue::open_waiting(&mut f.state, &LogFacts::default());
    f.key(KeyCode::Enter).await;
    assert_eq!(f.current_id(), id);
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&id).await;
    f.finish().await;
    assert!(f.tools.executed.lock().is_empty());
}

#[tokio::test]
async fn k05_long_metadata_keeps_exact_review_and_controls_reachable() {
    use rustain::domain::models::tool_call::ApprovalSource;
    static TOOL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!("mcp__{}__{}", "server".repeat(350), "operation".repeat(250))
    });
    let tool: &'static str = TOOL.as_str();
    let input = json!({"first": "INPUT-HEAD", "last": format!("{}\nMETADATA-EXACT-TAIL", "value ".repeat(180))});
    let mut f = Fixture::new(64).await;
    f.submit(vec![(tool, input.clone())]).await;
    let (event, snapshot) = f.wait_requested().await;
    let id = f.current_id();
    // Preserve the real request and exact snapshot while exercising long display metadata.
    let mut snapshot = snapshot.expect("private exact snapshot");
    snapshot.source = ApprovalSource::ForegroundSubagent {
        conversation_id: "long-source".into(),
        parent_tool_call_id: "parent-tool".into(),
        subagent_type: "source".repeat(400),
    };
    reducer::close(&mut f.state, Surface::Decision);
    f.state.decisions = Default::default();
    approval_bridge::apply(&mut f.state, event, Some(snapshot));
    assert_eq!(f.permission().request_id, id);
    for (width, height) in VIEWPORTS {
        if f.permission().reviewing {
            f.key(KeyCode::Esc).await;
        }
        let card = f.paint(width, height).join("\n");
        assert!(
            card.contains("Decision: Permission"),
            "{width}×{height}: {card}"
        );
        assert!(card.contains("Allow"), "{width}×{height}: {card}");
        f.key(KeyCode::Char('r')).await;
        let pages = f.review_pages(width, height).await;
        assert!(text(&pages).contains("METADATA-EXACT-TAIL"));
        assert!(f.permission().reviewing);
        for page in pages {
            let painted = page.join("\n");
            assert!(
                painted.contains("Decision: Permission"),
                "review remains in the permission frame: {painted}"
            );
            assert!(painted.contains("risk:"), "{width}×{height}: {painted}");
            assert!(painted.contains("Allow"), "{painted}");
            assert!(painted.contains("Deny"), "{painted}");
            assert!(painted.contains("Back"), "{painted}");
        }
    }
    f.key(KeyCode::Char('y')).await;
    f.wait_closed(&id).await;
    f.finish().await;
    assert_eq!(*f.tools.executed.lock(), vec![(tool.into(), input)]);
}

#[tokio::test]
async fn k05_cancelled_open_review_is_stale_until_closed() {
    let mut f = Fixture::new(64).await;
    f.submit(vec![(
        "Bash",
        json!({"command": "printf CANCELLED-CANARY"}),
    )])
    .await;
    f.wait_requested().await;
    let id = f.current_id();
    let source = f.permission().source.clone();
    f.key(KeyCode::Char('r')).await;
    f.runtime
        .cancel_by_source(&source, CancelReason::SourceAborted)
        .await;
    let (event, _) = f.next_approval().await;
    assert!(
        matches!(event, ApprovalRuntimeEvent::Cancelled { id: cancelled, .. } if cancelled == id)
    );
    assert!(
        decision_queue::current(&f.state, &LogFacts::default())
            .unwrap()
            .is_stale()
    );
    assert!(matches!(
        f.key(KeyCode::Char('y')).await,
        Some(InputAction::Consumed)
    ));
    assert_eq!(f.current_id(), id);
    assert!(f.tools.executed.lock().is_empty());
    let painted = f.paint(80, 24).join("\n");
    assert!(painted.contains("Close"), "{painted}");
    f.key(KeyCode::Esc).await;
    assert!(f.state.decisions.is_empty());
    assert!(f.state.modal_stack.is_empty());
    f.finish().await;
    assert!(f.tools.executed.lock().is_empty());
}

#[tokio::test]
async fn k05_answer_over_caller_does_not_promote_next_permission() {
    let mut f = Fixture::new(64).await;
    f.state.input_buffer = "protected draft".into();
    reducer::open(&mut f.state, Surface::Help);
    f.submit(vec![("Bash", json!({"command": "printf NESTED-A"}))])
        .await;
    let (event, _) = f.wait_requested().await;
    let a = request_id(&event);
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    decision_queue::open_waiting(&mut f.state, &LogFacts::default());
    f.key(KeyCode::Enter).await;
    assert_eq!(f.current_id(), a);
    assert_eq!(f.state.modal_stack.len(), 2);
    f.submit(vec![(
        "Write",
        json!({"file_path": "nested-b", "content": "B"}),
    )])
    .await;
    let (event, _) = f.wait_requested().await;
    let b = request_id(&event);
    assert_eq!(f.current_id(), a);
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&a).await;
    assert_eq!(
        f.state.modal_stack.top().map(|frame| frame.surface),
        Some(Surface::Help)
    );
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    assert!(
        f.runtime
            .pending_review(&b, ReviewScope::AllTools)
            .await
            .is_some()
    );
    f.key(KeyCode::Esc).await;
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    decision_queue::open_waiting(&mut f.state, &LogFacts::default());
    f.key(KeyCode::Enter).await;
    assert_eq!(f.current_id(), b);
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&b).await;
    f.finish().await;
    assert!(f.tools.executed.lock().is_empty());
}

#[tokio::test]
async fn k05_passive_dismissal_restores_feedback_without_advancing_its_queue() {
    // Feedback is a sub-context of the decision frame, not its own overlay.
    let mut f = Fixture::new(64).await;
    f.submit(vec![("Bash", json!({"command": "printf FEEDBACK-OWNER"}))])
        .await;
    f.wait_requested().await;
    let owner = f.current_id();
    f.key(KeyCode::Char('f')).await;
    for c in "preserved draft".chars() {
        f.key(KeyCode::Char(c)).await;
    }
    assert!(f.permission().feedback.is_some());
    reducer::open(&mut f.state, Surface::Help);
    f.submit(vec![(
        "Bash",
        json!({"command": "printf QUEUED-BEHIND-FEEDBACK"}),
    )])
    .await;
    let (event, _) = f.wait_requested().await;
    let queued = request_id(&event);
    f.key(KeyCode::Esc).await;
    assert_eq!(f.current_id(), owner);
    assert_eq!(
        f.permission().feedback.as_ref().unwrap().as_str(),
        "preserved draft"
    );
    assert_eq!(f.state.decisions.count(DecisionKind::Permission), 2);
    assert!(
        f.paint(60, 16)
            .join("\n")
            .contains("Feedback: preserved draft")
    );
    f.key(KeyCode::Enter).await;
    f.wait_closed(&owner).await;
    // Answering from the base promotes the next same-kind item (A9).
    assert_eq!(f.current_id(), queued);
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&queued).await;
    f.finish().await;
    assert!(f.tools.executed.lock().is_empty());
}

#[tokio::test]
async fn k05_tool_inspector_back_hands_off_to_queued_permission() {
    use rustain::domain::models::{InvocationStatus, MessageRole, StopReason};

    let mut f = Fixture::new(64).await;
    f.state.focus = FocusState::Input;
    f.state.input_buffer = "protected composer draft".into();
    let conversation = stage::make_conversation(
        vec![stage::make_msg("inspected-tool", MessageRole::Assistant)],
        vec![stage::make_turn(
            "inspected-tool",
            vec![stage::tool("Read", InvocationStatus::Success)],
            Some(StopReason::EndTurn),
        )],
    );
    stage::inspect_stage_tool(&mut f.state, &conversation, None);
    assert!(matches!(
        f.state.modal_stack.top().map(|frame| frame.surface),
        Some(Surface::ToolInspector(_))
    ));
    f.submit(vec![("Bash", json!({"command": "printf AFTER-PEEK"}))])
        .await;
    let (event, _) = f.wait_requested().await;
    let id = request_id(&event);
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    assert_eq!(f.state.decisions.count(DecisionKind::Permission), 1);
    assert!(matches!(
        f.key(KeyCode::Esc).await,
        Some(InputAction::Consumed)
    ));
    assert!(f.state.modal_stack.is_empty());
    assert!(f.state.tool_inspectors.is_empty());
    assert!(decision_queue::current(&f.state, &LogFacts::default()).is_none());
    decision_queue::open_waiting(&mut f.state, &LogFacts::default());
    f.key(KeyCode::Enter).await;
    assert_eq!(f.current_id(), id);
    f.key(KeyCode::Char('r')).await;
    assert!(text(&f.review_pages(60, 16).await).contains("AFTER-PEEK"));
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&id).await;
    f.finish().await;
    assert!(f.tools.executed.lock().is_empty());
}
