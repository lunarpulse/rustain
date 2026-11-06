//! K2 / M2: every applicable source/status/diff combination reaches real cells.
use super::support::*;
use rustain::adapters::tui::{
    keymap::Key,
    tool_inspector::{self, ToolTab},
};
use rustain::domain::models::{
    DiffKind, DiffLine, InvocationStatus as S, NotCapturedReason as R, ToolOutput, Turn, TurnPart,
    WriteDiffState as D,
};
use serde_json::{Value, json};

struct Case {
    label: &'static str,
    name: &'static str,
    args: Value,
    status: S,
    result: Option<(&'static str, D)>,
    diff: Option<Vec<&'static str>>,
}

#[test]
fn diff_tab_only_for_a_successful_edit_or_write() {
    let edit = || json!({"file_path":"notes.txt","old_string":"old","new_string":"new"});
    let write = || json!({"file_path":"notes.txt","content":"new\nnext"});
    let mut cases = vec![
        Case {
            label: "bash running",
            name: "Bash",
            args: json!({"command":"run"}),
            status: S::Running,
            result: None,
            diff: None,
        },
        Case {
            label: "bash success",
            name: "Bash",
            args: json!({"command":"run"}),
            status: S::Success,
            result: Some(("done", D::NotAWrite)),
            diff: None,
        },
        Case {
            label: "bash error",
            name: "Bash",
            args: json!({"command":"run"}),
            status: S::Error,
            result: Some(("failed", D::NotAWrite)),
            diff: None,
        },
        Case {
            label: "edit success",
            name: "Edit",
            args: edit(),
            status: S::Success,
            result: Some(("edited", D::NotAWrite)),
            diff: Some(vec!["- old", "+ new"]),
        },
        Case {
            label: "edit malformed",
            name: "Edit",
            args: json!({"old_string":"","new_string":"new"}),
            status: S::Success,
            result: Some(("edited", D::NotAWrite)),
            diff: None,
        },
        Case {
            label: "edit unchanged",
            name: "Edit",
            args: json!({"old_string":"same","new_string":"same"}),
            status: S::Success,
            result: Some(("unchanged", D::NotAWrite)),
            diff: None,
        },
        Case {
            label: "write new",
            name: "Write",
            args: write(),
            status: S::Success,
            result: Some(("written", D::NewFile)),
            diff: Some(vec!["+ new", "+ next"]),
        },
        Case {
            label: "write captured",
            name: "Write",
            args: write(),
            status: S::Success,
            result: Some((
                "written",
                D::Diff {
                    lines: vec![
                        DiffLine {
                            kind: DiffKind::Removed,
                            content: "old".into(),
                        },
                        DiffLine {
                            kind: DiffKind::Added,
                            content: "new".into(),
                        },
                    ],
                    more: 0,
                },
            )),
            diff: Some(vec!["- old", "+ new"]),
        },
        Case {
            label: "write unchanged",
            name: "Write",
            args: write(),
            status: S::Success,
            result: Some((
                "unchanged",
                D::Diff {
                    lines: vec![],
                    more: 0,
                },
            )),
            diff: Some(vec!["no line changes — the file content is unchanged"]),
        },
        Case {
            label: "write not a write",
            name: "Write",
            args: write(),
            status: S::Success,
            result: Some(("done", D::NotAWrite)),
            diff: None,
        },
        Case {
            label: "write malformed new",
            name: "Write",
            args: json!({"file_path":"notes.txt"}),
            status: S::Success,
            result: Some(("done", D::NewFile)),
            diff: None,
        },
    ];
    for (name, args) in [("Edit", edit()), ("Write", write())] {
        cases.push(Case {
            label: "running no diff",
            name,
            args: args.clone(),
            status: S::Running,
            result: None,
            diff: None,
        });
        cases.push(Case {
            label: "pending no diff",
            name,
            args: args.clone(),
            status: S::Pending,
            result: None,
            diff: None,
        });
        cases.push(Case {
            label: "error no diff",
            name,
            args: args.clone(),
            status: S::Error,
            result: Some(("failure", D::NewFile)),
            diff: None,
        });
        cases.push(Case {
            label: "cancelled no diff",
            name,
            args,
            status: S::Cancelled,
            result: Some(("cancelled", D::NewFile)),
            diff: None,
        });
    }
    for (reason, expected) in [
        (
            R::NoActiveCheckpoint,
            "overwrote 8 bytes — previous content not captured (no active checkpoint)",
        ),
        (
            R::SnapshotUnavailable,
            "wrote 8 bytes — previous content not captured (snapshot unavailable)",
        ),
        (
            R::SnapshotReadFailed,
            "wrote 8 bytes — previous content not captured (snapshot read failed)",
        ),
        (
            R::OriginalNotUtf8,
            "wrote 8 bytes — previous content not captured (original is not valid UTF-8)",
        ),
        (
            R::SupersededInBatch,
            "wrote 8 bytes — previous content not captured (superseded by another write in the same batch)",
        ),
        (
            R::HistoricalOrReattached,
            "wrote 8 bytes — previous content not captured (recorded before this feature, or re-projected on attach)",
        ),
    ] {
        cases.push(Case {
            label: "honest not captured reason",
            name: "Write",
            args: write(),
            status: S::Success,
            result: Some(("written", D::NotCaptured { reason })),
            diff: Some(vec![expected]),
        });
    }
    for case in cases {
        let (mut f, _) = Fixture::single(160, 30, case.name, case.args, case.status, case.result);
        f.open();
        let tab_rect = tool_inspector::layout::parts(f.frame_rect()).tabs;
        let strip = row(f.buffer(), tab_rect, tab_rect.y);
        assert!(
            strip.contains("[Output]") && strip.contains("Input") && strip.contains("Raw"),
            "{}: {strip}",
            case.label
        );
        assert_eq!(
            strip.contains("Diff"),
            case.diff.is_some(),
            "{}: {strip}",
            case.label
        );
        f.press(Key::Left);
        f.paint();
        assert!(
            row(f.buffer(), tab_rect, tab_rect.y).contains("[Output]"),
            "left stops at first"
        );
        for _ in 0..5 {
            f.press(Key::Right);
            f.paint();
        }
        let expected_last = if case.diff.is_some() {
            ToolTab::Diff
        } else {
            ToolTab::Raw
        };
        assert_eq!(f.tool().tab, expected_last, "{}", case.label);
        assert!(
            row(f.buffer(), tab_rect, tab_rect.y).contains(&format!("[{}]", expected_last.label()))
        );
        if let Some(expected) = case.diff {
            let body = (0..f.body_rect().height)
                .map(|n| f.body_row(n))
                .collect::<Vec<_>>()
                .join("\n");
            for text in expected {
                assert!(
                    body.contains(text),
                    "{}: missing {text:?} in {body:?}",
                    case.label
                );
            }
        } else {
            assert!(
                f.body_row(0).contains('{'),
                "Raw must actually paint a JSON record"
            );
        }
        for _ in 0..5 {
            f.press(Key::Left);
            f.paint();
        }
        assert_eq!(f.tool().tab, ToolTab::Output);
        assert!(row(f.buffer(), tab_rect, tab_rect.y).contains("[Output]"));
    }
}

#[test]
fn successful_completion_adds_diff_without_moving_the_active_tab() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    let id = call(
        &mut turn,
        "Edit",
        json!({"old_string":"old","new_string":"new"}),
        S::Running,
        None,
    );
    let part = part_for(&turn, &id);
    let mut f = Fixture::new(80, 24, Turn::new("unused".into(), 0));
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs.active_tab_mut().reducer.open_turn = Some(turn);
    f.paint();
    f.open();
    f.press(Key::Right);
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Input);
    let input_pixels = pixels(f.buffer(), f.body_rect());
    if let TurnPart::ToolInvocation {
        status, ended_at, ..
    } = f.invocation_mut(&id)
    {
        *status = S::Success;
        *ended_at = Some(1);
    }
    f.tabs
        .active_tab_mut()
        .reducer
        .open_turn
        .as_mut()
        .unwrap()
        .push_part(|id| TurnPart::ToolResult {
            id,
            refs: part,
            output: ToolOutput {
                content: "done".into(),
                is_error: false,
                diff: D::NotAWrite,
                images: vec![],
            },
        });
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Input);
    assert_eq!(pixels(f.buffer(), f.body_rect()), input_pixels);
    let rect = tool_inspector::layout::parts(f.frame_rect()).tabs;
    assert!(row(f.buffer(), rect, rect.y).contains("Diff"));
    f.press(Key::Right);
    f.press(Key::Right);
    f.paint();
    assert!(f.body_row(0).contains("- old"));
    assert!(f.body_row(1).contains("+ new"));
}

#[test]
fn empty_sources_paint_explicit_output_and_input_not_a_blank_body() {
    let (mut f, _) = Fixture::single(
        80,
        24,
        "Bash",
        json!({}),
        S::Success,
        Some(("", D::NotAWrite)),
    );
    f.open();
    assert!(f.body_row(0).contains("The tool returned no output."));
    f.press(Key::Right);
    f.paint();
    assert!(f.body_row(0).contains("No input."));
    f.press(Key::Right);
    f.paint();
    let body = (0..f.body_rect().height)
        .map(|n| f.body_row(n))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("\"tool\": \"Bash\""));
    assert!(body.contains("\"input\": {}"));
}

#[test]
fn raw_record_is_scoped_to_one_call_not_the_transcript_or_draft() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    prose(&mut turn, "UNRELATED-TRANSCRIPT-SECRET");
    call(
        &mut turn,
        "Bash",
        json!({"command":"unrelated"}),
        S::Success,
        Some(("UNRELATED-RESULT-SECRET", D::NotAWrite)),
    );
    prose(&mut turn, &rows(24));
    let current = call(
        &mut turn,
        "Bash",
        json!({"command":"current-command"}),
        S::Success,
        Some(("current-result", D::NotAWrite)),
    );
    let mut f = Fixture::new(80, 24, turn);
    f.focus_call(&current);
    f.h.state.input_buffer = "PRIVATE-DRAFT-SECRET".into();
    f.h.state.cursor_position = 5;
    f.open();
    f.press(Key::Right);
    f.press(Key::Right);
    f.paint();
    assert_eq!(f.tool().tab, ToolTab::Raw);
    let mut painted = String::new();
    for _ in 0..4 {
        painted.push_str(
            &(0..f.body_rect().height)
                .map(|n| f.body_row(n))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        f.press(Key::PageDown);
        f.paint();
    }
    assert!(painted.contains("current-command") && painted.contains("current-result"));
    for secret in [
        "UNRELATED-TRANSCRIPT-SECRET",
        "UNRELATED-RESULT-SECRET",
        "PRIVATE-DRAFT-SECRET",
    ] {
        assert!(
            !painted.contains(secret),
            "Raw serialized more than the current call: {secret}"
        );
    }
}

#[test]
fn review_r4_unchanged_large_sources_do_zero_payload_work() {
    use rustain::adapters::tui::tool_inspector::source::metrics;
    let large = format!("large-source\n{}", "x".repeat(1024 * 1024));
    let (mut f, _) = Fixture::single(
        80,
        24,
        "Write",
        json!({"content": large}),
        S::Success,
        Some((&large, D::NewFile)),
    );
    metrics::reset();
    f.open();
    for tab in [ToolTab::Output, ToolTab::Input, ToolTab::Raw, ToolTab::Diff] {
        while f.tool().tab != tab {
            f.press(Key::Right);
            f.compose();
        }
        f.compose();
        let (bytes, preparations) = metrics::counts();
        assert!(
            bytes >= 1024 * 1024 && preparations > 0,
            "cold {tab:?} must exercise the payload-work instrumentation"
        );
        metrics::reset();
        for _ in 0..4 {
            f.paint();
        }
        assert_eq!(metrics::counts(), (0, 0), "unchanged {tab:?}");
    }
    rustain::adapters::tui::presentation::reducer::open(
        &mut f.h.state,
        rustain::adapters::tui::presentation::Surface::Help,
    );
    metrics::reset();
    for _ in 0..4 {
        f.paint();
    }
    assert_eq!(metrics::counts(), (0, 0), "covered retained inspector");
}

#[test]
fn review_r4_mutations_and_same_id_replacement_update_cells_once() {
    use rustain::adapters::tui::tool_inspector::source::metrics;
    let (mut f, id) = Fixture::single(
        80,
        24,
        "Bash",
        json!({"command":"before"}),
        S::Success,
        Some(("before-output", D::NotAWrite)),
    );
    f.open();
    // In-place result change, not a reallocated payload or a new part.
    if let TurnPart::ToolResult { output, .. } =
        &mut f.tabs.active_tab_mut().conversation.turns[0].parts[1]
    {
        output.content.replace_range(..6, "after!");
    }
    metrics::reset();
    f.compose();
    assert!(f.body_row(0).contains("after!-output"));
    assert_eq!(metrics::counts().1, 1);
    f.compose();
    assert_eq!(metrics::counts().1, 1);
    f.press(Key::Right);
    f.compose();
    if let TurnPart::ToolInvocation { args, .. } = f.invocation_mut(&id) {
        args["command"] = json!("after-input");
    }
    metrics::reset();
    f.compose();
    assert!(f.body_row(1).contains("after-input"));
    assert_eq!(metrics::counts().1, 1);
    f.compose();
    assert_eq!(metrics::counts().1, 1);
    f.press(Key::Right);
    f.compose();
    if let TurnPart::ToolInvocation { status, .. } = f.invocation_mut(&id) {
        *status = S::Cancelled;
    }
    metrics::reset();
    f.compose();
    assert!(f.body_row(3).contains("cancelled"));
    assert_eq!(metrics::counts().1, 1);
    f.compose();
    assert_eq!(metrics::counts().1, 1);
    // Rehydrate identical conversation/turn/call identities with changed bytes.
    let mut replacement: rustain::domain::models::Conversation =
        serde_json::from_value(serde_json::to_value(f.conversation()).unwrap()).unwrap();
    if let TurnPart::ToolInvocation { args, .. } = &mut replacement.turns[0].parts[0] {
        args["command"] = json!("replacement-input");
    }
    f.tabs.active_tab_mut().conversation = replacement;
    metrics::reset();
    f.compose();
    let body = (0..f.body_rect().height)
        .map(|n| f.body_row(n))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("replacement-input"));
    assert_eq!(metrics::counts().1, 1);
    f.compose();
    assert_eq!(metrics::counts().1, 1);
}

#[test]
fn review_r4_live_tail_and_reducer_completion_invalidate_once() {
    use rustain::adapters::tui::tool_inspector::source::metrics;
    use rustain::domain::{models::StreamChunk, services::reducer};
    let (mut f, _) = Fixture::single(80, 24, "Bash", json!({}), S::Success, None);
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    reducer::reduce(
        &mut f.tabs.active_tab_mut().reducer,
        StreamChunk::ToolUse {
            id: "wire".into(),
            name: "Bash".into(),
            input: json!({"command":"live"}),
        },
        &*f.clock,
    );
    f.tabs
        .active_tab_mut()
        .reducer
        .set_tail("wire", "first-tail".into());
    {
        let tab = f.tabs.active_tab_mut();
        reducer::update_streaming_mirror(&tab.reducer, &mut tab.streaming);
    }
    f.paint();
    f.open();
    f.tabs
        .active_tab_mut()
        .reducer
        .set_tail("wire", "next-tail".into());
    metrics::reset();
    f.compose();
    assert!(f.body_row(1).contains("next-tail"));
    assert_eq!(metrics::counts().1, 1);
    f.compose();
    assert_eq!(metrics::counts().1, 1);
    reducer::reduce(
        &mut f.tabs.active_tab_mut().reducer,
        StreamChunk::ToolResult {
            id: "wire".into(),
            operator_private: false,
            content: "completed-output".into(),
            is_error: false,
            diff: D::NotAWrite,
            images: vec![],
        },
        &*f.clock,
    );
    metrics::reset();
    f.compose();
    assert!(f.body_row(0).contains("completed-output"));
    assert_eq!(f.tool().status, S::Success);
    assert_eq!(metrics::counts().1, 1);
    f.compose();
    assert_eq!(metrics::counts().1, 1);
}

#[test]
fn review_r5_long_history_refresh_allocates_no_temporary_call_ids() {
    use rustain::domain::models::turn::metrics;
    let (mut f, _) = Fixture::single(
        80,
        24,
        "Bash",
        json!({}),
        S::Success,
        Some(("target-output", D::NotAWrite)),
    );
    f.open();
    metrics::reset();
    let mut history = Turn::new("history".into(), 0);
    for _ in 0..10_000 {
        call(&mut history, "Bash", json!({}), S::Success, None);
    }
    assert_eq!(
        metrics::allocated_ids(),
        10_000,
        "the counter observes real formatted ID allocations in the fixture control"
    );
    f.tabs
        .active_tab_mut()
        .conversation
        .turns
        .insert(0, history);
    metrics::reset();
    for _ in 0..4 {
        f.compose();
        assert!(f.body_row(0).contains("target-output"));
    }
    assert_eq!(
        metrics::allocated_ids(),
        0,
        "no history-sized formatted ID temporaries"
    );
}
