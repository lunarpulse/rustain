//! K4 (slice 6-B): permission exactness in the local host. A request enters through
//! the real producer (`approval_bridge::apply`), is painted by the shared compose
//! boundary, and is answered through `presentation::route`. The attached twin of each
//! beat is `story21_6_attached_*` in `attach_keys.rs` / `attach_loop.rs`.

use std::sync::Arc;

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue;
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::clock::MockClock;
use rustain::domain::events::DomainKey;
use rustain::domain::models::ToolRisk;
use rustain::domain::models::tool_call::{ApprovalSource, RequestId};
use rustain::domain::services::approval_runtime::{ApprovalRuntimeEvent, PendingReview};
use rustain::infrastructure::runtime::approval_bridge;

use super::support::{advance, clocked, facts, key, paint, ready, route, row_of, screen, special};

fn request(state: &mut TuiState, tool: &str, input: serde_json::Value) -> RequestId {
    let id = RequestId::new();
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "c1".into(),
    };
    let review = PendingReview {
        id: id.clone(),
        source: source.clone(),
        tool: tool.into(),
        risk: ToolRisk::Elevated,
        input: Arc::new(input),
    };
    approval_bridge::apply(
        state,
        ApprovalRuntimeEvent::Requested {
            id: id.clone(),
            source,
            tool: tool.into(),
            risk: ToolRisk::Elevated,
            consent_card: None,
        },
        Some(review),
    );
    id
}

fn rows(screen: &str) -> Vec<String> {
    screen.lines().map(str::to_owned).collect()
}

/// A fresh 80x24 state with one painted, ready permission frame.
fn open(width: u16, height: u16) -> (TuiState, Arc<MockClock>, RequestId) {
    let (mut state, clock) = clocked(width, height);
    let id = request(&mut state, "Bash", serde_json::json!({"command": "ls -la"}));
    assert!(
        presentation::is_open(&state, Surface::Decision),
        "preempts idle Input"
    );
    paint(&mut state, &facts("c1"), width, height);
    ready(&mut state, &clock);
    (state, clock, id)
}

#[test]
fn k4_rail_and_body_are_exact_at_80_and_60_columns() {
    for (width, height) in [(80u16, 24u16), (60, 16)] {
        let (mut state, _clock) = clocked(width, height);
        request(&mut state, "Bash", serde_json::json!({"command": "ls -la"}));
        let buffer = paint(&mut state, &facts("c1"), width, height);
        let text = rows(&screen(&buffer));
        let joined = text.join("\n");
        assert!(joined.contains("Decision: Permission"), "{joined}");
        assert!(
            joined.contains("Needs you › Permission Bash | 1 pending"),
            "{joined}"
        );
        let body = text
            .iter()
            .find(|row| row.contains("command: ls -la"))
            .expect("body row");
        let inside_frame = body.trim_start().strip_prefix('│').expect("frame border");
        assert!(inside_frame.starts_with("│ command"), "{body}");
        assert!(
            joined.contains("[y] Allow  [s] Session  [a] Always"),
            "{joined}"
        );
        assert!(
            joined.contains("[n] Deny  [f] Deny + feedback  [r] Review"),
            "{joined}"
        );
    }
}

#[test]
fn k4_a_long_input_shows_four_rows_then_the_review_marker_and_review_shows_it_all() {
    let (mut state, clock) = clocked(80, 24);
    request(
        &mut state,
        "Bash",
        serde_json::json!({"command": "one\ntwo\nthree\nfour\nLATE-SUFFIX"}),
    );
    let buffer = paint(&mut state, &facts("c1"), 80, 24);
    let text = screen(&buffer);
    assert!(
        text.contains("… more not shown — [r] Review first"),
        "{text}"
    );
    assert!(!text.contains("LATE-SUFFIX"), "{text}");
    ready(&mut state, &clock);
    route(&mut state, &key('r'), &facts("c1"));
    let buffer = paint(&mut state, &facts("c1"), 80, 24);
    let text = screen(&buffer);
    assert!(
        text.contains("Exact tool input (not run yet) · Bash"),
        "{text}"
    );
    assert!(text.contains("LATE-SUFFIX"), "{text}");
    assert!(
        text.contains("[y] Allow  [n] Deny  [f] Deny + feedback  [Esc] Back  PgUp/PgDn scroll"),
        "{text}"
    );
    // Esc goes back to the SAME undecided request, it never denies.
    route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    let buffer = paint(&mut state, &facts("c1"), 80, 24);
    assert!(screen(&buffer).contains("[r] Review"));
    assert!(presentation::is_open(&state, Surface::Decision));
    assert_eq!(decision_queue::waiting(&state), 1);
}

#[test]
fn k4_esc_defers_and_never_denies_in_both_hosts() {
    let (mut state, _clock, _id) = open(80, 24);
    let effect = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    assert_eq!(effect, Some(RouteEffect::Consumed), "Esc decides nothing");
    assert!(
        !presentation::is_open(&state, Surface::Decision),
        "the frame closed"
    );
    assert_eq!(
        decision_queue::waiting(&state),
        1,
        "the request stays queued and counted"
    );
    assert!(state.decisions.items().all(|item| item.deferred));
}

#[test]
fn k4_a_key_on_first_sight_is_dropped_and_after_the_window_decides_once() {
    let (mut state, clock) = clocked(80, 24);
    request(&mut state, "Bash", serde_json::json!({"command": "ls"}));
    paint(&mut state, &facts("c1"), 80, 24);
    advance(&clock, std::time::Duration::from_millis(499));
    assert_eq!(
        route(&mut state, &key('y'), &facts("c1")),
        Some(RouteEffect::Consumed)
    );
    assert_eq!(
        decision_queue::waiting(&state),
        1,
        "dropped inside the inert window"
    );
    advance(&clock, std::time::Duration::from_millis(2));
    assert_eq!(
        route(&mut state, &key('y'), &facts("c1")),
        Some(RouteEffect::Action(InputAction::PermissionAllow))
    );
    assert_eq!(decision_queue::waiting(&state), 0);
    assert!(!presentation::is_open(&state, Surface::Decision));
    // A second press finds nothing to answer.
    assert_ne!(
        route(&mut state, &key('y'), &facts("c1")),
        Some(RouteEffect::Action(InputAction::PermissionAllow))
    );
}

#[test]
fn k4_each_rail_key_maps_to_its_existing_effect() {
    for (c, expected) in [
        ('y', InputAction::PermissionAllow),
        ('s', InputAction::PermissionSessionAllow),
        ('a', InputAction::PermissionAlwaysAllow),
        ('n', InputAction::PermissionDeny),
    ] {
        let (mut state, _clock, _id) = open(80, 24);
        assert_eq!(
            route(&mut state, &key(c), &facts("c1")),
            Some(RouteEffect::Action(expected)),
            "{c}"
        );
    }
}

#[test]
fn k4_two_requests_are_fifo_and_a_base_answer_opens_the_second() {
    let (mut state, clock, first) = open(80, 24);
    let second = request(
        &mut state,
        "Write",
        serde_json::json!({"file_path": "/w/a"}),
    );
    assert_eq!(decision_queue::waiting(&state), 2, "one frame, two items");
    let buffer = paint(&mut state, &facts("c1"), 80, 24);
    assert!(screen(&buffer).contains("| 2 pending"));
    ready(&mut state, &clock);
    let _ = route(&mut state, &key('n'), &facts("c1"));
    // The legacy same-kind sequence: answered from the base, the next one opens at once.
    assert!(presentation::is_open(&state, Surface::Decision));
    let remaining: Vec<_> = state
        .decisions
        .items()
        .map(|item| item.source.clone())
        .collect();
    assert_eq!(remaining, vec![second.0.clone()]);
    assert_ne!(first, second);
}

#[test]
fn k4_answering_over_help_restores_help_and_does_not_open_the_next() {
    let (mut state, clock) = clocked(80, 24);
    presentation::reducer::open(&mut state, Surface::Help);
    request(&mut state, "Bash", serde_json::json!({"command": "ls"}));
    request(
        &mut state,
        "Write",
        serde_json::json!({"file_path": "/w/a"}),
    );
    assert!(presentation::is_open(&state, Surface::Decision));
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    let _ = route(&mut state, &key('y'), &facts("c1"));
    assert_eq!(
        state.modal_stack.top().unwrap().surface,
        Surface::Help,
        "Help restored"
    );
    assert_eq!(
        decision_queue::waiting(&state),
        1,
        "the second waits, never opened by surprise"
    );
}

#[test]
fn k4_display_controls_are_not_painted_and_the_input_is_unchanged() {
    let (mut state, _clock) = clocked(80, 24);
    let id = request(
        &mut state,
        "Bash",
        serde_json::json!({"command": "echo \u{1b}[2Jhi\u{7}\u{202e}x"}),
    );
    let buffer = paint(&mut state, &facts("c1"), 80, 24);
    let text = screen(&buffer);
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}') && !text.contains('\u{202e}'),
        "{text:?}"
    );
    assert!(text.contains("echo"));
    let item = state.decisions.items().next().unwrap();
    match &item.subject {
        decision_queue::DecisionSubject::Permission(payload) => {
            assert_eq!(payload.request_id, id);
            match &payload.input {
                decision_queue::PermissionInput::Ready(value) => {
                    assert_eq!(value["command"], "echo \u{1b}[2Jhi\u{7}\u{202e}x");
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn k4_deny_with_feedback_types_in_the_frame_field_and_leaves_the_composer_alone() {
    let (mut state, _clock, _id) = open(80, 24);
    let composer = (state.input_buffer.clone(), state.cursor_position);
    let _ = route(&mut state, &key('f'), &facts("c1"));
    for c in "use rg".chars() {
        let _ = route(&mut state, &key(c), &facts("c1"));
    }
    let buffer = paint(&mut state, &facts("c1"), 80, 24);
    let text = screen(&buffer);
    assert!(text.contains("Feedback: use rg"), "{text}");
    assert!(text.contains("[Enter] Send  [Esc] Back"), "{text}");
    assert_eq!(
        (state.input_buffer.clone(), state.cursor_position),
        composer
    );
    assert_eq!(
        route(&mut state, &special(DomainKey::Enter), &facts("c1")),
        Some(RouteEffect::Action(InputAction::FeedbackInputSubmit))
    );
    assert_eq!(decision_queue::waiting(&state), 0);
    assert_eq!((state.input_buffer, state.cursor_position), composer);
}

#[test]
fn k4_esc_leaves_the_feedback_field_first_and_defers_second() {
    let (mut state, _clock, _id) = open(80, 24);
    let _ = route(&mut state, &key('f'), &facts("c1"));
    let _ = route(&mut state, &key('x'), &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    assert!(
        presentation::is_open(&state, Surface::Decision),
        "first Esc leaves the field"
    );
    let buffer = paint(&mut state, &facts("c1"), 80, 24);
    assert!(screen(&buffer).contains("[r] Review"));
    let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    assert!(
        !presentation::is_open(&state, Surface::Decision),
        "second Esc defers"
    );
    assert_eq!(decision_queue::waiting(&state), 1);
}

#[test]
fn k4_a_settled_request_turns_the_open_frame_stale_with_close_only() {
    let (mut state, _clock, id) = open(80, 24);
    approval_bridge::apply(
        &mut state,
        ApprovalRuntimeEvent::Cancelled {
            id,
            reason: rustain::domain::services::approval_runtime::CancelReason::SourceAborted,
        },
        None,
    );
    let buffer = paint(&mut state, &facts("c1"), 80, 24);
    let text = screen(&buffer);
    assert!(
        text.contains("approval refused: that request is no longer pending"),
        "{text}"
    );
    assert!(text.contains("[Esc] Close"), "{text}");
    assert_ne!(
        route(&mut state, &key('y'), &facts("c1")),
        Some(RouteEffect::Action(InputAction::PermissionAllow))
    );
    let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    assert_eq!(decision_queue::waiting(&state), 0);
    assert!(row_of(&paint(&mut state, &facts("c1"), 80, 24), "Decision:").is_none());
}

#[test]
fn k4_permission_and_review_keep_body_and_complete_rails_with_full_composer() {
    let (mut state, clock) = clocked(60, 16);
    state.input_buffer = (0..12)
        .map(|n| format!("draft line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    state.focus = rustain::domain::models::FocusState::Chat;
    request(
        &mut state,
        "Bash",
        serde_json::json!({"command": "full-composer-input"}),
    );
    let text = screen(&paint(&mut state, &facts("c1"), 60, 16));
    assert!(text.contains("│ command: full-composer-input"), "{text}");
    assert!(
        text.contains("[y] Allow  [s] Session  [a] Always"),
        "{text}"
    );
    assert!(
        text.contains("[n] Deny  [f] Deny + feedback  [r] Review"),
        "{text}"
    );
    assert!(text.contains("status-row-sentinel"), "{text}");
    ready(&mut state, &clock);
    route(&mut state, &key('r'), &facts("c1"));
    let text = screen(&paint(&mut state, &facts("c1"), 60, 16));
    assert!(text.contains("│ command: full-composer-input"), "{text}");
    for label in [
        "[y] Allow",
        "[n] Deny",
        "[f] Deny + feedback",
        "[Esc] Back",
        "PgUp/PgDn scroll",
    ] {
        assert!(text.contains(label), "{text}");
    }
    assert!(text.contains("status-row-sentinel"), "{text}");
}

#[tokio::test]
async fn k4_read_only_tab_dispatches_all_permission_answers_to_the_runtime() {
    use rustain::adapters::noop::NoOpApprovalPersistence;
    use rustain::domain::models::ApprovalOutcome;
    use rustain::domain::services::approval_runtime::{ApprovalRuntime, ReviewScope};

    for answer in ['y', 's', 'a', 'n', 'f'] {
        let runtime = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
        let source = ApprovalSource::ForegroundTurn {
            conversation_id: "c1".into(),
        };
        let input = serde_json::json!({"command": "echo ok"});
        let (id, response) = runtime
            .request(
                source.clone(),
                "Bash".into(),
                input,
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let id = id.unwrap();
        let review = runtime
            .pending_review(&id, ReviewScope::AllTools)
            .await
            .unwrap();
        let (mut state, clock) = clocked(80, 24);
        approval_bridge::apply(
            &mut state,
            ApprovalRuntimeEvent::Requested {
                id,
                source,
                tool: "Bash".into(),
                risk: ToolRisk::Elevated,
                consent_card: None,
            },
            Some(review),
        );
        paint(&mut state, &facts("c1"), 80, 24);
        ready(&mut state, &clock);
        let effect = route(&mut state, &key(answer), &facts("c1"));
        let action = if answer == 'f' {
            route(&mut state, &key('x'), &facts("c1"));
            route(&mut state, &special(DomainKey::Enter), &facts("c1"))
        } else {
            effect
        };
        let Some(RouteEffect::Action(action)) = action else {
            panic!("missing answer action for {answer}")
        };
        // This is the exact read-only-tab predicate in the live dispatcher.
        let read_only = true;
        assert!(
            !(read_only && action.is_mutation()),
            "{answer} was discarded"
        );
        approval_bridge::handle_action(&mut state, &action, &runtime).await;
        let outcome = response.await.unwrap().outcome;
        match answer {
            'y' => assert!(matches!(outcome, ApprovalOutcome::Once)),
            's' => assert!(matches!(outcome, ApprovalOutcome::AlwaysTool { .. })),
            'a' => assert!(matches!(outcome, ApprovalOutcome::AlwaysAndSave { .. })),
            'n' => assert!(matches!(
                outcome,
                ApprovalOutcome::Reject { feedback: None }
            )),
            'f' => assert!(
                matches!(&outcome, ApprovalOutcome::Reject { feedback: Some(s) } if s == "x")
            ),
            _ => unreachable!(),
        }
    }
}
