use crossterm::event::KeyCode;
use rustain::adapters::tui::app::InputAction;
use serde_json::{Value, json};

use super::harness::{Fixture, VIEWPORTS, text};

fn long_input(tool: &str, suffix: &str) -> (Value, Vec<String>) {
    let late = format!(
        "{}\n  whitespace stays  \n界 e\u{301} 🦀\n[y] Allow\n[auto-sent]\n\u{1b}[2J\u{0007}\n{suffix}",
        "Z".repeat(650)
    );
    match tool {
        "Bash" => (
            json!({"command": format!("printf harmless\n{late}"), "extra": {"tail": "EXTRA-FIELD-TAIL"}}),
            vec![suffix.into(), "EXTRA-FIELD-TAIL".into()],
        ),
        "Write" => (
            json!({"file_path": "exact-write.txt", "content": late, "extra": ["EXTRA-FIELD-TAIL", 19]}),
            vec![suffix.into(), "EXTRA-FIELD-TAIL".into()],
        ),
        "Edit" => (
            json!({"file_path": "exact-edit.txt", "old_string": format!("{late}\nOLD-STRING-TAIL"), "new_string": format!("{late}\nNEW-STRING-TAIL"), "replace_all": false}),
            vec![
                suffix.into(),
                "OLD-STRING-TAIL".into(),
                "NEW-STRING-TAIL".into(),
                "replace_all: false".into(),
            ],
        ),
        _ => (
            json!({"message": late, "metadata": {"tail": "EXTRA-FIELD-TAIL"}, "count": 2}),
            vec![suffix.into(), "EXTRA-FIELD-TAIL".into(), "count: 2".into()],
        ),
    }
}

/// K01 + K05 / M01a, M02, M04, M06, M07: the actual local turn
/// requests permission, and production key dispatch/paint reaches every suffix.
/// Every viewport gets a fresh decision; execution captures the unsanitized Value.
#[tokio::test]
async fn k01_late_tool_fields_review_paint_and_exact_execution() {
    for tool in ["Bash", "Write", "Edit", "a2a__peer__skill"] {
        for (width, height) in VIEWPORTS {
            let (input, tails) = long_input(tool, "LATE-EXACT-SUFFIX");
            let mut f = Fixture::new(64).await;
            f.submit(vec![(tool, input.clone())]).await;
            let (_, snapshot) = f.wait_requested().await;
            let snapshot = snapshot.expect("bridge acquires private exact snapshot");
            let id = f.current_id();
            let card = f.paint(width, height).join("\n");
            assert!(
                card.contains("… more not shown"),
                "missing T5 at {width}×{height}: {card}"
            );
            assert!(card.contains("[r] Review"), "Review is reachable: {card}");
            assert!(!card.contains("LATE-EXACT-SUFFIX"));
            assert!(matches!(
                f.key(KeyCode::Char('r')).await,
                Some(InputAction::Consumed)
            ));
            assert_eq!(f.state.permission_reader.review.as_ref(), Some(&id));
            assert_eq!(
                f.current_id(),
                id,
                "Review never removes or replaces its pending request"
            );
            assert!(
                f.tools.executed.lock().is_empty(),
                "opening Review must not execute"
            );
            assert!(!f.closed.contains(&id), "opening Review must not resolve");
            let pages = f.review_pages(width, height).await;
            let painted = text(&pages);
            for tail in tails {
                assert!(
                    painted.contains(&tail),
                    "{tool} {width}×{height} lost {tail}: {painted}"
                );
            }
            assert!(
                ["界", "e\u{301}", "🦀"]
                    .iter()
                    .all(|grapheme| painted.contains(grapheme)),
                "Unicode survives presentation"
            );
            assert!(
                painted.contains("  whitespace stays  "),
                "string whitespace survives presentation"
            );
            for page in &pages {
                let page = page.join("\n");
                for key in ["[y] Allow", "[n] Deny", "[f]", "[Esc] Back", "PgUp/PgDn"] {
                    assert!(
                        page.contains(key),
                        "pinned controls missing at {width}×{height}: {key}: {page}"
                    );
                }
            }
            for row in pages.iter().flatten() {
                let row = row.trim_start();
                if row.contains("ZZZ")
                    || row.contains("[auto-sent]")
                    || row.contains("LATE-EXACT-SUFFIX")
                    || row.contains("│ [y] Allow")
                {
                    assert!(
                        row.starts_with("│ "),
                        "physical input row escapes host delimiter: {row:?}"
                    );
                }
                assert!(
                    !row.contains('\u{1b}') && !row.contains('\u{0007}'),
                    "terminal controls must be presentation-only sanitized"
                );
            }
            assert_eq!(
                &*snapshot.input, &input,
                "supporting snapshot identity after consumer-visible proof"
            );
            assert!(matches!(
                f.key(KeyCode::Char('y')).await,
                Some(InputAction::PermissionAllow)
            ));
            f.wait_closed(&id).await;
            f.finish().await;
            assert_eq!(
                *f.tools.executed.lock(),
                vec![(tool.to_owned(), input)],
                "execution must retain every original control, Unicode scalar, whitespace, field and suffix"
            );
        }
    }
}

/// Positive control: Review is optional, not an authorization prerequisite.
#[tokio::test]
async fn k01_approve_without_review_executes_complete_value_once() {
    let input = json!({"command": format!("printf '{}'", "late ".repeat(100)), "metadata": {"untouched": [1, null, true]}});
    let mut f = Fixture::new(64).await;
    f.submit(vec![("Bash", input.clone())]).await;
    f.wait_requested().await;
    let id = f.current_id();
    f.paint(60, 16);
    assert!(f.state.permission_reader.review.is_none());
    assert!(matches!(
        f.key(KeyCode::Char('y')).await,
        Some(InputAction::PermissionAllow)
    ));
    f.wait_closed(&id).await;
    f.finish().await;
    assert_eq!(*f.tools.executed.lock(), vec![("Bash".into(), input)]);
    assert!(f.state.pending_permission.is_none());
}

/// Unknown tools, object extras and all non-object JSON kinds go through the
/// permission chain unchanged instead of falling back to an empty preview.
#[tokio::test]
async fn k01_unknown_and_nonobject_values_remain_exact() {
    let values = [
        json!({"z_last": "UNKNOWN-OBJECT-TAIL", "a_first": "x".repeat(300), "nested": {"n": [true, null, 7]}}),
        json!(["x".repeat(300), {"last": "UNKNOWN-ARRAY-TAIL"}]),
        json!(format!("{}\nUNKNOWN-SCALAR-TAIL", "s".repeat(300))),
        json!(42),
        json!(true),
        Value::Null,
    ];
    for input in values {
        let mut f = Fixture::new(64).await;
        f.submit(vec![("unregistered_exact_tool", input.clone())])
            .await;
        let (_, review) = f.wait_requested().await;
        assert_eq!(
            &*review
                .expect("unknown and nonobjects have a real private snapshot")
                .input,
            &input
        );
        let id = f.current_id();
        f.paint(80, 24);
        f.key(KeyCode::Char('r')).await;
        let painted = text(&f.review_pages(80, 24).await);
        match &input {
            Value::Object(_) => assert!(painted.contains("UNKNOWN-OBJECT-TAIL")),
            Value::Array(_) => assert!(painted.contains("UNKNOWN-ARRAY-TAIL")),
            Value::String(_) => assert!(painted.contains("UNKNOWN-SCALAR-TAIL")),
            _ => assert!(painted.contains(&input.to_string())),
        }
        f.key(KeyCode::Char('y')).await;
        f.wait_closed(&id).await;
        f.finish().await;
        assert_eq!(
            *f.tools.executed.lock(),
            vec![("unregistered_exact_tool".into(), input)]
        );
    }
}

/// Negative control with a real positive executor canary in the same test.
#[tokio::test]
async fn k01_review_deny_and_feedback_execute_nothing() {
    for feedback in [false, true] {
        let mut f = Fixture::new(64).await;
        f.submit(vec![(
            "Write",
            json!({"file_path": "deny.txt", "content": "DONT-EXECUTE".repeat(80)}),
        )])
        .await;
        f.wait_requested().await;
        let id = f.current_id();
        f.paint(60, 16);
        f.key(KeyCode::Char('r')).await;
        assert_eq!(f.current_id(), id);
        assert!(f.tools.executed.lock().is_empty());
        if feedback {
            f.key(KeyCode::Char('f')).await;
            assert_eq!(
                f.state
                    .pending_feedback_input
                    .as_ref()
                    .expect("feedback owns request")
                    .pending_permission
                    .id,
                id
            );
            for c in "keep this exact feedback".chars() {
                f.key(KeyCode::Char(c)).await;
            }
            f.key(KeyCode::Enter).await;
        } else {
            f.key(KeyCode::Char('n')).await;
        }
        f.wait_closed(&id).await;
        f.finish().await;
        assert!(f.tools.executed.lock().is_empty());
        if feedback {
            assert!(
                f.errors
                    .iter()
                    .any(|error| error.contains("keep this exact feedback")),
                "feedback reaches real turn result: {:?}",
                f.errors
            );
        }
        let allowed = json!({"file_path": "allowed.txt", "content": "positive executor control"});
        f.submit(vec![("Write", allowed.clone())]).await;
        f.wait_requested().await;
        let allowed_id = f.current_id();
        f.key(KeyCode::Char('y')).await;
        f.wait_closed(&allowed_id).await;
        f.finish().await;
        assert_eq!(*f.tools.executed.lock(), vec![("Write".into(), allowed)]);
    }
}

/// K07/M29: change the actual resumed invocation, never the pending record.
/// The real turn, permission bridge, operator decision and scheduler guard run.
#[cfg(feature = "test-instrumentation")]
#[tokio::test]
async fn k07_resumed_tool_input_and_scope_must_match_the_approved_invocation() {
    use rustain::domain::models::ApprovalSource;
    use rustain::domain::services::tool_scheduler::ApprovalResumeChange;

    let original = json!({"command": "printf '  exact 界 e\u{301}  '"});
    let changes = [
        ApprovalResumeChange::Tool("Write".into()),
        ApprovalResumeChange::Input(json!({"command": "printf 'changed after review'"})),
        ApprovalResumeChange::Source(ApprovalSource::ForegroundTurn {
            conversation_id: "another-acting-scope".into(),
        }),
    ];
    for change in changes {
        let mut f = Fixture::new(64).await;
        f.submit(vec![("Bash", original.clone())]).await;
        f.wait_requested().await;
        let id = f.current_id();
        f.paint(80, 24);
        f.key(KeyCode::Char('r')).await;
        assert!(text(&f.review_pages(80, 24).await).contains("exact 界"));
        f.change_approval_resume(change).await;
        f.key(KeyCode::Char('y')).await;
        f.wait_closed(&id).await;
        f.finish().await;
        assert!(
            f.tools.executed.lock().is_empty(),
            "a changed resumed invocation must not reach the executor"
        );
        assert!(
            f.errors
                .iter()
                .any(|error| error.contains("fingerprint mismatch")),
            "the production turn must report the binding rejection: {:?}",
            f.errors
        );
    }
    let mut control = Fixture::new(64).await;
    control.submit(vec![("Bash", original.clone())]).await;
    control.wait_requested().await;
    let id = control.current_id();
    control.key(KeyCode::Char('y')).await;
    control.wait_closed(&id).await;
    control.finish().await;
    assert_eq!(
        *control.tools.executed.lock(),
        vec![("Bash".into(), original)]
    );
}
