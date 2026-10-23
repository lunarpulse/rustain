use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rustain::adapters::tui::app::{self, InputAction};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{FocusState, ImageAttachment};
use rustain::domain::services::approval_runtime::{ApprovalRuntimeEvent, ReviewScope};
use rustain::infrastructure::runtime::approval_bridge;
use serde_json::json;

use super::harness::{Fixture, VIEWPORTS, press, request_id, text};

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
    assert_eq!(f.state.permission_reader.review.as_ref(), Some(&a));
    assert_eq!(f.state.permission_queue.len(), 1);
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
    assert_eq!(f.state.permission_reader.review.as_ref(), Some(&a));
    assert!(matches!(
        f.key(KeyCode::Esc).await,
        Some(InputAction::Consumed)
    ));
    assert!(f.state.permission_reader.review.is_none());
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
    assert_eq!(
        f.state
            .pending_feedback_input
            .as_ref()
            .expect("feedback owns A")
            .pending_permission
            .id,
        a
    );
    for c in "feedback draft".chars() {
        f.key(KeyCode::Char(c)).await;
    }
    f.submit(vec![("Bash", json!({"command": "printf QUEUED-C"}))])
        .await;
    let (event, _) = f.wait_requested().await;
    let c = request_id(&event);
    let feedback = f
        .state
        .pending_feedback_input
        .as_ref()
        .expect("arrival must not steal feedback");
    assert_eq!(feedback.pending_permission.id, a);
    assert_eq!(feedback.buffer, "feedback draft");
    assert!(f.state.pending_permission.is_none());
    assert_eq!(f.state.permission_queue.len(), 2);
    let feedback_paint = f.paint(60, 16).join("\n");
    assert!(
        feedback_paint.contains("Feedback: feedback draft"),
        "{feedback_paint}"
    );
    assert!(feedback_paint.contains("[Esc] cancel"), "{feedback_paint}");
    assert!(matches!(
        f.key(KeyCode::Esc).await,
        Some(InputAction::FeedbackInputCancel)
    ));
    assert_eq!(f.current_id(), a);
    assert_eq!(
        f.state.permission_reader.review.as_ref(),
        Some(&a),
        "feedback Esc returns to the same pinned Review"
    );
    let restored_paint = f.paint(60, 16).join("\n");
    assert!(
        restored_paint.contains("Exact tool input (not run yet)"),
        "{restored_paint}"
    );
    f.key(KeyCode::Esc).await;
    assert!(f.state.permission_reader.review.is_none());
    assert_eq!(f.current_id(), a);
    assert!(matches!(
        f.key(KeyCode::Esc).await,
        Some(InputAction::PermissionDeny)
    ));
    f.wait_closed(&a).await;
    assert_eq!(f.current_id(), b);
    f.key(KeyCode::Esc).await;
    f.wait_closed(&b).await;
    assert_eq!(f.current_id(), c);
    f.key(KeyCode::Esc).await;
    f.wait_closed(&c).await;
    f.finish().await;
    assert!(f.tools.executed.lock().is_empty());
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
    let permission_focus = f.state.focus.clone();
    f.state.focus = FocusState::Chat;
    f.state.pending_large_image = Some(ImageAttachment {
        media_type: "image/png".into(),
        data: "confirmation-fixture".into(),
    });
    assert!(matches!(
        f.key(KeyCode::Char('y')).await,
        Some(InputAction::ImageConfirmAttach)
    ));
    assert_eq!(f.current_id(), id);
    assert_eq!(f.state.permission_reader.review.as_ref(), Some(&id));
    assert!(
        f.runtime
            .pending_review(&id, ReviewScope::AllTools)
            .await
            .is_some()
    );
    assert!(f.tools.executed.lock().is_empty());
    // Image attachment effects belong to the image dispatcher, not approval;
    // removing that fixture models its completed higher-priority confirmation.
    f.state.pending_large_image = None;
    f.state.focus = permission_focus;
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
                            | InputAction::PermissionDenyFeedback
                    )
                ),
                "modified {key} must not decide: {modifiers:?}"
            );
            assert_eq!(f.current_id(), id);
            assert!(f.state.pending_feedback_input.is_none());
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
    assert_eq!(f.state.permission_reader.review.as_ref(), Some(&id));
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
        Some(InputAction::Consumed)
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
    approval_bridge::apply(&mut other_consumer, event_a, snapshot_a);
    f.paint(80, 24);
    f.key(KeyCode::Char('r')).await;
    f.submit(vec![(
        "Write",
        json!({"file_path": "b", "content": "B-MUST-NOT-EXECUTE"}),
    )])
    .await;
    let (event_b, _) = f.wait_requested().await;
    let b = request_id(&event_b);
    let raw = press(KeyCode::Char('y'));
    let domain =
        app::convert_crossterm_event(&raw, &Default::default()).expect("real raw conversion");
    let decision = app::handle_operator_input(&mut other_consumer, &raw, &domain)
        .expect("real decision dispatch");
    assert!(matches!(decision, InputAction::PermissionAllow));
    approval_bridge::handle_action(&mut other_consumer, &decision, &f.runtime).await;
    let (resolved, _) = f.next_approval().await;
    assert!(matches!(resolved, ApprovalRuntimeEvent::Resolved { id, .. } if id == a));
    assert!(
        f.state.permission_reader.review.is_none(),
        "resolved Review must retire immediately"
    );
    assert_eq!(f.current_id(), b, "only B may own the next key");
    let rows = f.paint(80, 24).join("\n");
    assert!(!rows.contains("A-EXECUTES"));
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
    assert_eq!(
        f.state.permission_queue.len() + usize::from(f.state.pending_permission.is_some()),
        REQUESTS
    );
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
    assert!(f.state.pending_permission.is_none());
    assert_eq!(f.state.focus, FocusState::Input);
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
    assert!(
        f.state.pending_permission.is_none(),
        "approval arrival cannot steal a higher modal"
    );
    assert_eq!(f.state.permission_queue.len(), 1);
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
    // Only the unrelated plan modal is seeded here. Its own dispatcher owns
    // dismissal; approval admission is the shared production advance helper.
    f.state.pending_plan_approval = None;
    f.state.focus = FocusState::Input;
    approval_bridge::advance(&mut f.state);
    assert_eq!(f.current_id(), id);
    assert!(f.paint(60, 16).join("\n").contains("AFTER-PLAN-CANARY"));
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
        assert_eq!(f.state.permission_reader.review.as_ref(), Some(&id));
        assert!(f.tools.executed.lock().is_empty());
    }
    f.key(KeyCode::Char('y')).await;
    f.wait_closed(&id).await;
    f.finish().await;
    assert_eq!(*f.tools.executed.lock(), vec![("Write".into(), input)]);
}

#[tokio::test]
async fn k05_passive_overlay_dismissals_surface_deferred_approval() {
    use rustain::domain::models::OverlayType;

    for (overlay, dismiss) in [
        ("help", KeyCode::Esc),
        ("help", KeyCode::Char('?')),
        ("which-key", KeyCode::Esc),
        ("which-key", KeyCode::Char('!')),
        ("palette", KeyCode::Esc),
    ] {
        let mut f = Fixture::new(64).await;
        f.state.focus = FocusState::Chat;
        f.state.input_buffer = "composer draft".into();
        match overlay {
            "help" => {
                f.state.help_overlay.open(FocusState::Chat);
                f.state.focus = FocusState::Overlay(OverlayType::Help);
            }
            "which-key" => {
                f.state.which_key.open(FocusState::Chat);
                f.state.focus = FocusState::Overlay(OverlayType::WhichKey);
            }
            _ => {
                f.state.command_palette.open(FocusState::Chat);
                f.state.focus = FocusState::Overlay(OverlayType::CommandPalette);
            }
        }
        let input = json!({"command": "printf AFTER-PASSIVE-OVERLAY"});
        f.submit(vec![("Bash", input.clone())]).await;
        let (event, _) = f.wait_requested().await;
        let id = request_id(&event);
        assert!(f.state.pending_permission.is_none());
        assert_eq!(f.state.permission_queue.len(), 1);
        assert!(matches!(f.key(dismiss).await, Some(InputAction::Consumed)));
        assert_eq!(f.current_id(), id, "{overlay} dismissal must hand off");
        assert_eq!(f.state.permission_queue.len(), 0);
        assert!(f.paint(60, 16).join("\n").contains("AFTER-PASSIVE-OVERLAY"));
        assert_eq!(f.state.input_buffer, "composer draft");
        f.key(KeyCode::Char('y')).await;
        f.wait_closed(&id).await;
        f.finish().await;
        assert_eq!(*f.tools.executed.lock(), vec![("Bash".into(), input)]);
    }
}

#[tokio::test]
async fn k05_passive_dismissal_preserves_remaining_modal_and_higher_owner() {
    use rustain::domain::models::OverlayType;

    let mut f = Fixture::new(64).await;
    f.state.which_key.open(FocusState::Chat);
    f.state
        .help_overlay
        .open(FocusState::Overlay(OverlayType::WhichKey));
    f.state.focus = FocusState::Overlay(OverlayType::Help);
    f.submit(vec![(
        "Bash",
        json!({"command": "printf AFTER-HIGHER-OWNER"}),
    )])
    .await;
    let (event, _) = f.wait_requested().await;
    let id = request_id(&event);
    f.key(KeyCode::Esc).await;
    assert_eq!(f.state.focus, FocusState::Overlay(OverlayType::WhichKey));
    assert!(f.state.pending_permission.is_none());
    assert_eq!(f.state.permission_queue.len(), 1);
    f.state.pending_large_image = Some(ImageAttachment {
        media_type: "image/png".into(),
        data: "higher-image-confirmation".into(),
    });
    f.key(KeyCode::Esc).await;
    assert_eq!(f.state.focus, FocusState::Chat);
    assert!(
        f.state.pending_permission.is_none(),
        "image owner still blocks"
    );
    assert_eq!(f.state.permission_queue.len(), 1);
    assert!(f.tools.executed.lock().is_empty());
    f.state.pending_large_image = None;
    f.state.help_overlay.open(FocusState::Chat);
    f.state.focus = FocusState::Overlay(OverlayType::Help);
    f.key(KeyCode::Esc).await;
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
    f.wait_requested().await;
    let id = f.current_id();
    // Source attribution is display metadata, independent of decision identity.
    f.state.pending_permission.as_mut().unwrap().source = ApprovalSource::ForegroundSubagent {
        conversation_id: "long-source".into(),
        parent_tool_call_id: "parent-tool".into(),
        subagent_type: "source".repeat(400),
    };
    for (width, height) in VIEWPORTS {
        f.state.permission_reader.back();
        let card = f.paint(width, height).join("\n");
        assert!(card.contains("[r] Review"), "{width}×{height}: {card}");
        assert!(card.contains("[y] Allow"), "{width}×{height}: {card}");
        f.key(KeyCode::Char('r')).await;
        let pages = f.review_pages(width, height).await;
        assert!(text(&pages).contains("METADATA-EXACT-TAIL"));
        assert!(f.state.permission_reader.viewport_rows > 0);
        for page in pages {
            let painted = page.join("\n");
            assert!(
                painted.contains("risk:"),
                "risk stays visible before source elision: {painted}"
            );
            assert!(painted.contains("[y] Allow"), "{painted}");
            assert!(painted.contains("[n] Deny"), "{painted}");
            assert!(painted.contains("[Esc] Back"), "{painted}");
        }
    }
    f.key(KeyCode::Char('y')).await;
    f.wait_closed(&id).await;
    f.finish().await;
    assert_eq!(*f.tools.executed.lock(), vec![(tool.into(), input)]);
}

#[tokio::test]
async fn k05_passive_dismissal_restores_feedback_without_advancing_its_queue() {
    use rustain::domain::models::OverlayType;
    use rustain::domain::models::visual::ConfirmationType;

    let mut f = Fixture::new(64).await;
    f.submit(vec![("Bash", json!({"command": "printf FEEDBACK-OWNER"}))])
        .await;
    f.wait_requested().await;
    let owner = f.current_id();
    f.key(KeyCode::Char('f')).await;
    for c in "preserved draft".chars() {
        f.key(KeyCode::Char(c)).await;
    }
    let feedback_focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::PermissionFeedback,
    ));
    f.state.help_overlay.open(feedback_focus.clone());
    f.state.focus = FocusState::Overlay(OverlayType::Help);
    f.submit(vec![(
        "Bash",
        json!({"command": "printf QUEUED-BEHIND-FEEDBACK"}),
    )])
    .await;
    let (event, _) = f.wait_requested().await;
    let queued = request_id(&event);
    f.key(KeyCode::Esc).await;
    assert_eq!(f.state.focus, feedback_focus);
    let feedback = f.state.pending_feedback_input.as_ref().unwrap();
    assert_eq!(feedback.pending_permission.id, owner);
    assert_eq!(feedback.buffer, "preserved draft");
    assert!(f.state.pending_permission.is_none());
    assert_eq!(f.state.permission_queue.len(), 1);
    assert!(
        f.paint(60, 16)
            .join("\n")
            .contains("Feedback: preserved draft")
    );
    f.key(KeyCode::Enter).await;
    f.wait_closed(&owner).await;
    assert_eq!(f.current_id(), queued);
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&queued).await;
    f.finish().await;
    assert!(f.tools.executed.lock().is_empty());
}

#[tokio::test]
async fn k05_peek_dismissal_hands_off_to_queued_permission() {
    let mut f = Fixture::new(64).await;
    f.state.focus = FocusState::Chat;
    f.state
        .tool_block_states
        .entry("peeked-tool".into())
        .or_default()
        .peek_active = true;
    f.submit(vec![("Bash", json!({"command": "printf AFTER-PEEK"}))])
        .await;
    let (event, _) = f.wait_requested().await;
    let id = request_id(&event);
    assert!(f.state.pending_permission.is_none());
    assert_eq!(f.state.permission_queue.len(), 1);
    assert!(matches!(
        f.key(KeyCode::Esc).await,
        Some(InputAction::Consumed)
    ));
    assert!(!f.state.tool_block_states["peeked-tool"].peek_active);
    assert_eq!(f.current_id(), id);
    assert!(f.paint(60, 16).join("\n").contains("AFTER-PEEK"));
    f.key(KeyCode::Char('n')).await;
    f.wait_closed(&id).await;
    f.finish().await;
    assert!(f.tools.executed.lock().is_empty());
}
