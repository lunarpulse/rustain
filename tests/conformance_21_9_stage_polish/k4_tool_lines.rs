//! K4 (AC9-AC11): every tool call reads like a sentence on the Stage, in both
//! hosts, with stats that are true or absent, text that is safe to paint and
//! geometry that never overflows. The witnesses are the production chat-pane
//! front doors (`render_with_search`, `render_attached`) read back from a
//! `TestBackend`; expected rows are literals, and the change counts are checked
//! against an independent line-diff counter that shares no code with the
//! production one.
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};
use rustain::adapters::daemon::protocol::ToolInputWithheld;
use rustain::adapters::tui::{
    state::TabRenderState,
    theme::Theme,
    widgets::{
        chat_pane,
        tool_block::{render_tool_block_lines, tool_block_height},
    },
};
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    Conversation, InvocationStatus, MessageRole, StopReason, StreamingState, ToolCallInfo,
    ToolOutput, ToolResultInfo, TurnPart, ViewState, WriteDiffState,
};
use serde_json::{Value, json};

use crate::conformance_21_12_markdown::support::{row_text, rows};
use crate::render_helpers::{make_conversation, make_msg, make_turn};

/// The `MockClock` wall time every pane here paints with (ms).
const NOW: u64 = 1_700_000_000_000;
/// A finished call took 0.3 s.
const START: u64 = 1_000_000;
const END: u64 = 1_000_300;

type Outcome = Option<(bool, &'static str, WriteDiffState)>;

struct Case {
    name: &'static str,
    tool: &'static str,
    input: Value,
    /// `(is_error, content, diff)` of the finished call; `None` = still running.
    result: Outcome,
    /// The Stage row after the lead span (`┄ `).
    row: &'static str,
}

fn ok(diff: WriteDiffState) -> Outcome {
    Some((false, "done", diff))
}

fn lines_of(n: usize) -> String {
    (1..=n).map(|i| format!("l{i}\n")).collect()
}

fn big_diff() -> WriteDiffState {
    let old: String = (0..300).map(|i| format!("old {i}\n")).collect();
    let new: String = (0..300).map(|i| format!("new {i}\n")).collect();
    WriteDiffState::from_original(old.as_bytes(), &new)
}

fn info(tool: &str, input: Value, result: &Outcome) -> ToolCallInfo {
    let running = result.is_none();
    ToolCallInfo {
        id: format!("call-{tool}"),
        name: tool.to_owned(),
        input,
        result: result
            .as_ref()
            .map(|(is_error, content, diff)| ToolResultInfo {
                content: (*content).to_owned(),
                is_error: *is_error,
                diff: diff.clone(),
                images: Vec::new(),
            }),
        started_at_ms: Some(if running { NOW - 2_000 } else { START }),
        completed_at_ms: (!running).then_some(END),
        status: running.then(|| "● Executing".to_owned()),
        provider_id: None,
    }
}

fn legacy_conversation(tool: ToolCallInfo) -> Conversation {
    let mut msg = make_msg("m1", MessageRole::Assistant);
    msg.tool_calls = vec![tool];
    make_conversation(vec![msg], Vec::new())
}

fn turn_conversation(tool: &str, input: Value, result: &Outcome) -> Conversation {
    let mut turn = make_turn("t1", Vec::new(), Some(StopReason::EndTurn));
    let failed = result.as_ref().is_some_and(|(is_error, ..)| *is_error);
    let status = match result {
        None => InvocationStatus::Running,
        Some(_) if failed => InvocationStatus::Error,
        Some(_) => InvocationStatus::Success,
    };
    let invocation = turn.push_part(|id| TurnPart::ToolInvocation {
        id,
        tool: tool.to_owned(),
        args: input,
        status,
        started_at: if result.is_none() {
            (NOW - 2_000) as i64
        } else {
            START as i64
        },
        ended_at: result.is_some().then_some(END as i64),
    });
    if let Some((is_error, content, diff)) = result {
        turn.push_part(|id| TurnPart::ToolResult {
            id,
            refs: invocation,
            output: ToolOutput {
                content: (*content).to_owned(),
                is_error: *is_error,
                diff: diff.clone(),
                images: Vec::new(),
            },
        });
    }
    make_conversation(vec![make_msg("t1", MessageRole::Assistant)], vec![turn])
}

struct Painted {
    buffer: Buffer,
}

fn local(conversation: &Conversation, root: Option<&Path>, width: u16) -> Painted {
    let clock = MockClock::at_wall_ms(NOW as i64);
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
    terminal
        .draw(|frame| {
            let mut tab = TabRenderState {
                workspace_root: root.map(Arc::from),
                ..TabRenderState::default()
            };
            chat_pane::render_with_search(
                frame,
                Rect::new(0, 0, width, 24),
                conversation,
                None,
                &StreamingState::default(),
                &ViewState::default(),
                &clock,
                0,
                true,
                &Theme::dark(),
                &mut tab,
                &BTreeMap::new(),
                None,
                None,
                &[],
                &[],
                None,
                None,
                None,
                Default::default(),
            );
        })
        .unwrap();
    Painted {
        buffer: terminal.backend().buffer().clone(),
    }
}

fn attached(
    conversation: &Conversation,
    root: Option<&Path>,
    withheld: &HashMap<String, ToolInputWithheld>,
    width: u16,
) -> Painted {
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
    terminal
        .draw(|frame| {
            let mut tab = TabRenderState {
                workspace_root: root.map(Arc::from),
                ..TabRenderState::default()
            };
            chat_pane::render_attached(
                frame,
                Rect::new(0, 0, width, 24),
                conversation,
                &StreamingState::default(),
                &ViewState::default(),
                &Theme::dark(),
                &mut tab,
                &BTreeMap::new(),
                withheld,
            );
        })
        .unwrap();
    Painted {
        buffer: terminal.backend().buffer().clone(),
    }
}

fn rows_of(painted: &Painted) -> Vec<String> {
    rows(&painted.buffer)
}

/// The painted tool rows (header and error rows), gutter and focus arrow removed.
fn tool_rows(painted: &Painted) -> Vec<String> {
    rows_of(painted)
        .into_iter()
        .map(|row| {
            let row = row.strip_prefix("▶ ").unwrap_or(&row);
            row.strip_prefix("│ ").unwrap_or(row).to_owned()
        })
        .filter(|row| row.starts_with("┄ ") || row.starts_with("┃ "))
        .collect()
}

fn header(painted: &Painted) -> String {
    let rows = tool_rows(painted);
    assert!(
        !rows.is_empty(),
        "no tool row painted: {:?}",
        rows_of(painted)
    );
    rows[0].clone()
}

fn cases() -> Vec<Case> {
    let capped = big_diff();
    let complete = WriteDiffState::from_original(b"a\nx\nc\n", "a\nb\nc\n");
    vec![
        Case {
            name: "read, no range",
            tool: "Read",
            input: json!({"file_path": "src/x.rs"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Read src/x.rs · 0.3s",
        },
        Case {
            name: "read, offset and limit",
            tool: "Read",
            input: json!({"file_path": "src/x.rs", "offset": 0, "limit": 80}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Read src/x.rs:1-80 · 0.3s",
        },
        Case {
            name: "read, offset only takes the executor default limit",
            tool: "Read",
            input: json!({"file_path": "src/x.rs", "offset": 9}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Read src/x.rs:10-2009 · 0.3s",
        },
        Case {
            name: "read, limit only starts at line 1",
            tool: "Read",
            input: json!({"file_path": "src/x.rs", "limit": 50}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Read src/x.rs:1-50 · 0.3s",
        },
        Case {
            name: "edit",
            tool: "Edit",
            input: json!({
                "file_path": "src/parser.rs",
                "old_string": "a\nb\nc",
                "new_string": "a\nx\ny\nz\nc",
            }),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Edit src/parser.rs +3 \u{2212}1 · 0.3s",
        },
        Case {
            name: "edit with identical strings shows the path only",
            tool: "Edit",
            input: json!({"file_path": "src/p.rs", "old_string": "a", "new_string": "a"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Edit src/p.rs · 0.3s",
        },
        Case {
            name: "edit with a missing string shows the path only",
            tool: "Edit",
            input: json!({"file_path": "src/p.rs", "old_string": "a"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Edit src/p.rs · 0.3s",
        },
        Case {
            name: "write, new file",
            tool: "Write",
            input: json!({"file_path": "src/new.rs", "content": lines_of(40)}),
            result: ok(WriteDiffState::NewFile),
            row: "┄ ✓ Write src/new.rs +40 · 0.3s",
        },
        Case {
            name: "write, complete diff",
            tool: "Write",
            input: json!({"file_path": "f.rs", "content": "a\nb\nc\n"}),
            result: ok(complete),
            row: "┄ ✓ Write f.rs +1 \u{2212}1 · 0.3s",
        },
        Case {
            name: "write, capped diff shows the line total",
            tool: "Write",
            input: json!({"file_path": "big.rs", "content": lines_of(300)}),
            result: ok(capped),
            row: "┄ ✓ Write big.rs 300 lines · 0.3s",
        },
        Case {
            name: "write, original not captured",
            tool: "Write",
            input: json!({"file_path": "f.rs", "content": "a\nb\nc"}),
            result: ok(WriteDiffState::NotCaptured {
                reason: rustain::domain::models::NotCapturedReason::SnapshotUnavailable,
            }),
            row: "┄ ✓ Write f.rs 3 lines · 0.3s",
        },
        Case {
            name: "write, nothing changed",
            tool: "Write",
            input: json!({"file_path": "f.rs", "content": "a\n"}),
            result: ok(WriteDiffState::Diff {
                lines: Vec::new(),
                more: 0,
            }),
            row: "┄ ✓ Write f.rs no change · 0.3s",
        },
        Case {
            name: "write, still running",
            tool: "Write",
            input: json!({"file_path": "f.rs", "content": "a\nb\nc"}),
            result: None,
            row: "┄ ● Write f.rs 3 lines · 2.0s",
        },
        Case {
            name: "write, one line",
            tool: "Write",
            input: json!({"file_path": "f.rs", "content": "a"}),
            result: None,
            row: "┄ ● Write f.rs 1 line · 2.0s",
        },
        Case {
            name: "bash, one line",
            tool: "Bash",
            input: json!({"command": "cargo test --lib"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Bash cargo test --lib · 0.3s",
        },
        Case {
            name: "bash, several lines",
            tool: "Bash",
            input: json!({"command": "cargo test\n  --lib\n"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Bash cargo test … · 0.3s",
        },
        Case {
            name: "apply_patch, one file",
            tool: "apply_patch",
            input: json!({"patch": "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** End Patch\n"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ apply_patch src/a.rs · 0.3s",
        },
        Case {
            name: "apply_patch, three files",
            tool: "apply_patch",
            input: json!({"patch": "*** Begin Patch\n*** Add File: src/new.rs\n+x\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Delete File: old.rs\n*** End Patch\n"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ apply_patch src/new.rs +2 files · 0.3s",
        },
        Case {
            name: "an MCP tool",
            tool: "mcp__srv__lookup",
            input: json!({"query": "rust lifetimes"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ [srv] lookup rust lifetimes · 0.3s",
        },
        Case {
            name: "an unknown tool with nested JSON only shows no JSON",
            tool: "Frobnicate",
            input: json!({"options": {"a": 1, "b": ["x"]}, "count": 3}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Frobnicate · 0.3s",
        },
        Case {
            name: "an unknown tool takes its first string value",
            tool: "Frobnicate",
            input: json!({"count": 3, "label": "second", "zeta": "last"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Frobnicate second · 0.3s",
        },
        Case {
            name: "preferred keys beat key order",
            tool: "Frobnicate",
            input: json!({"aaa": "early", "query": "wanted"}),
            result: ok(WriteDiffState::NotAWrite),
            row: "┄ ✓ Frobnicate wanted · 0.3s",
        },
    ]
}

/// No row of the old shape survives: no JSON, arrow, status word or closing border.
fn assert_new_shape(row: &str, name: &str) {
    for old in ["{\"", "→", "Success", "Executing", "running..."] {
        assert!(!row.contains(old), "{name}: {old:?} in {row:?}");
    }
    assert!(
        !row.trim_end().ends_with('┄'),
        "{name}: closing border in {row:?}"
    );
    assert_eq!(
        row.matches('┄').count(),
        1,
        "{name}: only the lead is a border: {row:?}"
    );
}

#[test]
fn every_tool_kind_reads_like_a_sentence_in_both_hosts_and_both_paths() {
    for case in cases() {
        let call = info(case.tool, case.input.clone(), &case.result);
        let legacy = legacy_conversation(call);
        let turn = turn_conversation(case.tool, case.input.clone(), &case.result);
        let none = HashMap::new();
        let paths = [
            ("local legacy", local(&legacy, None, 80)),
            ("local turn", local(&turn, None, 80)),
            ("attached", attached(&legacy, None, &none, 80)),
        ];
        for (path, painted) in paths {
            let row = header(&painted);
            if path == "attached" && case.result.is_none() {
                // `render_attached` paints with the system clock: the running
                // elapsed is a pattern, everything before it is exact.
                let (head, elapsed) = row.rsplit_once(" · ").expect("an elapsed segment");
                let (want, _) = case.row.rsplit_once(" · ").unwrap();
                assert_eq!(head, want, "{} ({path})", case.name);
                assert!(
                    elapsed.ends_with('s') && elapsed.trim_end_matches('s').parse::<f64>().is_ok(),
                    "{}: elapsed {elapsed:?}",
                    case.name
                );
            } else {
                assert_eq!(row, case.row, "{} ({path})", case.name);
            }
            assert_new_shape(&row, case.name);
        }
    }
}

#[test]
fn statuses_are_glyphs_and_errors_keep_their_rows() {
    for (status, glyph) in [
        ("⋯ Validating", "⋯"),
        ("⧖ Scheduled", "⧖"),
        ("? Awaiting approval", "?"),
        ("● Executing", "●"),
        ("⊘ Cancelled", "⊘"),
    ] {
        let mut call = info("Bash", json!({"command": "ls"}), &None);
        call.status = Some(status.to_owned());
        let row = header(&local(&legacy_conversation(call), None, 80));
        assert_eq!(row, format!("┄ {glyph} Bash ls · 2.0s"), "{status}");
    }
    // Pending: no chip, no glyph.
    let mut pending = info("Bash", json!({"command": "ls"}), &None);
    pending.status = None;
    assert_eq!(
        header(&local(&legacy_conversation(pending), None, 80)),
        "┄ Bash ls · 2.0s"
    );
    // An error keeps the border lead, the ✗ glyph and one row per error line.
    let failed: Outcome = Some((true, "boom\nsecond line", WriteDiffState::NotAWrite));
    let call = info("Bash", json!({"command": "ls"}), &failed);
    let legacy = local(&legacy_conversation(call.clone()), None, 80);
    let turn = local(
        &turn_conversation("Bash", json!({"command": "ls"}), &failed),
        None,
        80,
    );
    for painted in [&legacy, &turn] {
        assert_eq!(
            tool_rows(painted),
            ["┃ ✗ Bash ls · 0.3s", "┃ boom", "┃ second line"]
        );
    }
    // The height rule is unchanged: header plus one row per error line.
    assert_eq!(tool_block_height(&call), 3);
    assert_eq!(
        render_tool_block_lines(&call, &Theme::dark(), 80, &MockClock::at_wall_ms(0)).len(),
        3
    );
}

#[test]
fn withheld_input_keeps_its_notice_and_never_the_arguments() {
    for (withheld, expected) in [
        (
            ToolInputWithheld::PeerTier,
            "┄ ✓ Bash input withheld — not shared with peer attachments · 0.3s",
        ),
        (
            ToolInputWithheld::TooLarge { bytes: 2_097_152 },
            "┄ ✓ Bash input too large to show here (2.0 MiB) · 0.3s",
        ),
    ] {
        let call = info(
            "Bash",
            json!({"command": "secret-command"}),
            &ok(WriteDiffState::NotAWrite),
        );
        let map = HashMap::from([(call.id.clone(), withheld)]);
        let painted = attached(&legacy_conversation(call), None, &map, 100);
        let row = header(&painted);
        assert_eq!(row, expected);
        assert!(
            !rows_of(&painted)
                .iter()
                .any(|r| r.contains("secret-command"))
        );
    }
}

/// Independent line-diff counter: LCS over `\n`-split lines (a trailing `\r`
/// dropped), `+` = new − lcs, `−` = old − lcs. Shares nothing with `diff_stat`.
fn independent_stat(old: &str, new: &str) -> (usize, usize) {
    let split = |text: &str| -> Vec<String> {
        let mut lines: Vec<String> = text
            .split('\n')
            .map(|line| line.trim_end_matches('\r').to_owned())
            .collect();
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    };
    let (a, b) = (split(old), split(new));
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            table[i][j] = if a[i - 1] == b[j - 1] {
                table[i - 1][j - 1] + 1
            } else {
                table[i - 1][j].max(table[i][j - 1])
            };
        }
    }
    let lcs = table[a.len()][b.len()];
    (b.len() - lcs, a.len() - lcs)
}

#[test]
fn edit_line_counts_exact_added_and_removed() {
    let fixtures: [(&str, &str, &str); 8] = [
        ("pure add", "a\nb\n", "a\nb\nc\nd\n"),
        ("pure delete", "a\nb\nc\nd\n", "a\nd\n"),
        ("replace", "a\nb\nc\n", "a\nX\nc\n"),
        (
            "crlf",
            "one\r\ntwo\r\nthree\r\n",
            "one\r\n2\r\nthree\r\nfour\r\n",
        ),
        (
            "multi-hunk",
            "a\nb\nc\nd\ne\nf\ng\n",
            "a\nB\nc\nd\ne\nF\nF2\ng\n",
        ),
        ("reorder", "a\nb\nc\n", "c\nb\na\n"),
        ("whole rewrite", "a\nb\n", "x\ny\nz\n"),
        // Mixed endings: the old side is CRLF, the new side LF plus one added line.
        ("mixed endings", "one\r\ntwo\r\n", "one\ntwo\nthree\r\n"),
    ];
    for (name, old, new) in fixtures {
        let (added, removed) = independent_stat(old, new);
        let call = info(
            "Edit",
            json!({"file_path": "f.rs", "old_string": old, "new_string": new}),
            &ok(WriteDiffState::NotAWrite),
        );
        let expected = format!("┄ ✓ Edit f.rs +{added} \u{2212}{removed} · 0.3s");
        let row = header(&local(&legacy_conversation(call), None, 80));
        assert_eq!(row, expected, "{name}");
    }
}

#[test]
fn capped_write_shows_line_total_not_partial_stat() {
    let diff = big_diff();
    let WriteDiffState::Diff { lines, more } = &diff else {
        panic!("the fixture diff is a capped Diff: {diff:?}");
    };
    assert!(*more > 0, "the fixture really is capped");
    // The stored rows alone would give a partial count; the line must not use it.
    let partial_added = lines
        .iter()
        .filter(|row| row.kind == rustain::domain::models::DiffKind::Added)
        .count();
    assert!(partial_added > 0 && partial_added < 300);
    let call = info(
        "Write",
        json!({"file_path": "big.rs", "content": lines_of(300)}),
        &ok(diff.clone()),
    );
    let row = header(&local(&legacy_conversation(call), None, 80));
    assert_eq!(row, "┄ ✓ Write big.rs 300 lines · 0.3s");
    assert!(!row.contains('+'), "{row}");
}

#[test]
fn controls_and_bidi_in_arguments_never_reach_cells() {
    let nasty =
        "x\u{1b}[31mred\u{1b}[0m\u{1b}]0;title\u{7}\u{9b}31m\u{202e}evil\u{2066}end\u{85}\u{0}";
    let cases: [(&str, Value); 8] = [
        (
            "Bash",
            json!({"command": format!("echo {nasty}\nsecond line")}),
        ),
        ("Read", json!({"file_path": format!("src/{nasty}.rs")})),
        (
            "Write",
            json!({"file_path": format!("src/{nasty}.rs"), "content": "a"}),
        ),
        (
            "Edit",
            json!({"file_path": format!("{nasty}.rs"), "old_string": "a", "new_string": "b"}),
        ),
        (
            "apply_patch",
            json!({"patch": format!("*** Update File: {nasty}\n-a\n+b\n")}),
        ),
        (
            "Frobnicate",
            json!({"description": format!("one\n\ttwo {nasty}")}),
        ),
        ("mcp__srv__look\u{1b}up", json!({"query": nasty})),
        (nasty, json!({"query": "q"})),
    ];
    for (tool, input) in cases {
        let call = info(tool, input.clone(), &ok(WriteDiffState::NotAWrite));
        // The builder's own text is clean: the terminal buffer drops control and
        // zero-width characters by itself, so cells alone prove too little.
        let theme = Theme::dark();
        for line in render_tool_block_lines(&call, &theme, 100, &MockClock::at_wall_ms(NOW as i64))
        {
            let text = line.to_string();
            assert!(
                !text.chars().any(|c| c.is_control()
                    || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
                "{tool:?}: control or bidi char in the built row {text:?}"
            );
            assert!(!text.contains('\n'), "{tool:?}: newline in the built row");
        }
        let turn = turn_conversation(tool, input, &ok(WriteDiffState::NotAWrite));
        let none = HashMap::new();
        for (path, painted) in [
            (
                "local legacy",
                local(&legacy_conversation(call.clone()), None, 100),
            ),
            ("local turn", local(&turn, None, 100)),
            (
                "attached",
                attached(&legacy_conversation(call), None, &none, 100),
            ),
        ] {
            for cell in painted.buffer.content() {
                let symbol = cell.symbol();
                assert!(
                    !symbol.chars().any(|c| c.is_control()
                        || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
                    "{tool:?} ({path}): control or bidi char in a cell: {symbol:?}"
                );
            }
            assert_eq!(
                tool_rows(&painted).len(),
                1,
                "{tool:?} ({path}): a newline split the row"
            );
        }
    }
    // Positive control: the visible text around the controls survives.
    let call = info(
        "Bash",
        json!({"command": "echo \u{1b}[31mred\u{1b}[0m"}),
        &ok(WriteDiffState::NotAWrite),
    );
    let row = header(&local(&legacy_conversation(call), None, 80));
    assert!(row.contains("echo red"), "{row}");
}

#[test]
fn workspace_paths_display_relative_and_others_as_given() {
    let root = Path::new("/ws/proj");
    for (path, shown) in [
        ("/ws/proj/src/x.rs", "src/x.rs"),
        ("/ws/proj/x.rs", "x.rs"),
        ("/ws/proj", "."),
        ("/ws/projx/src/x.rs", "/ws/projx/src/x.rs"),
        ("/other/x.rs", "/other/x.rs"),
        ("src/x.rs", "src/x.rs"),
    ] {
        for (tool, input) in [
            ("Read", json!({"file_path": path})),
            ("Frobnicate", json!({"path": path})),
        ] {
            let call = info(tool, input, &ok(WriteDiffState::NotAWrite));
            let row = header(&local(&legacy_conversation(call), Some(root), 80));
            assert_eq!(row, format!("┄ ✓ {tool} {shown} · 0.3s"), "{path}");
        }
    }
    // Without a root nothing is rewritten.
    let call = info(
        "Read",
        json!({"file_path": "/ws/proj/src/x.rs"}),
        &ok(WriteDiffState::NotAWrite),
    );
    assert_eq!(
        header(&local(&legacy_conversation(call), None, 80)),
        "┄ ✓ Read /ws/proj/src/x.rs · 0.3s"
    );
}

#[test]
fn rows_never_exceed_the_width_and_drop_pieces_in_order() {
    const PATH: &str = "src/adapters/tui/widgets/chat_pane/reasoning.rs";
    let call = info(
        "Edit",
        json!({"file_path": PATH, "old_string": "a", "new_string": "b\nc"}),
        &ok(WriteDiffState::NotAWrite),
    );
    let pinned: [(u16, &str); 5] = [
        (
            130,
            "┄ ✓ Edit src/adapters/tui/widgets/chat_pane/reasoning.rs +2 \u{2212}1 · 0.3s",
        ),
        (
            80,
            "┄ ✓ Edit src/adapters/tui/widgets/chat_pane/reasoning.rs +2 \u{2212}1 · 0.3s",
        ),
        // The elapsed piece goes first...
        (
            68,
            "┄ ✓ Edit src/adapters/tui/widgets/chat_pane/reasoning.rs +2 \u{2212}1",
        ),
        // ...then the path loses its head...
        (
            60,
            "┄ ✓ Edit …/adapters/tui/widgets/chat_pane/reasoning.rs +2 \u{2212}1",
        ),
        // ...keeping whole segments when it can.
        (40, "┄ ✓ Edit …/chat_pane/reasoning.rs +2 \u{2212}1"),
    ];
    for (width, expected) in pinned {
        let row = header(&local(&legacy_conversation(call.clone()), None, width));
        assert_eq!(row, expected, "width {width}");
    }
    // Last, the detail goes, with the path ellipsized down to what is left.
    let row = header(&local(&legacy_conversation(call.clone()), None, 20));
    assert_eq!(row, "┄ ✓ Edit …asoning.rs");

    // Every width from the narrowest that holds a lead, a glyph and one verb
    // cell, every case: the row fits.
    let theme = Theme::dark();
    let clock = MockClock::at_wall_ms(NOW as i64);
    for case in cases() {
        let call = info(case.tool, case.input.clone(), &case.result);
        for width in 5..=130u16 {
            let line = render_tool_block_lines(&call, &theme, width, &clock).remove(0);
            assert!(
                line.width() <= usize::from(width),
                "{} at {width}: {} cells in {:?}",
                case.name,
                line.width(),
                line.to_string()
            );
        }
    }
    // The painted rows at the five pinned widths fit as well.
    for width in [20u16, 40, 60, 80, 130] {
        for case in cases() {
            let call = info(case.tool, case.input.clone(), &case.result);
            let painted = local(&legacy_conversation(call), None, width);
            let y = (0..24)
                .find(|y| row_text(&painted.buffer, *y).starts_with("┄ "))
                .expect("a painted tool row");
            assert!(
                unicode_width::UnicodeWidthStr::width(row_text(&painted.buffer, y).as_str())
                    <= usize::from(width),
                "{} at {width}",
                case.name
            );
        }
    }
}

#[test]
fn a_wide_grapheme_target_is_clipped_on_a_grapheme_boundary() {
    let call = info(
        "Bash",
        json!({"command": "echo 界界界界界界界界界界界界 e\u{301}e\u{301}e\u{301}e\u{301}"}),
        &ok(WriteDiffState::NotAWrite),
    );
    let theme = Theme::dark();
    for width in 12..=40u16 {
        let line =
            render_tool_block_lines(&call, &theme, width, &MockClock::at_wall_ms(0)).remove(0);
        assert!(line.width() <= usize::from(width), "width {width}: {line}");
    }
}

#[test]
fn heights_match_the_old_rule_for_every_fixture() {
    let theme = Theme::dark();
    for case in cases() {
        let call = info(case.tool, case.input.clone(), &case.result);
        let rendered = render_tool_block_lines(&call, &theme, 80, &MockClock::at_wall_ms(0));
        assert_eq!(rendered.len(), tool_block_height(&call), "{}", case.name);
        assert_eq!(
            tool_block_height(&call),
            1,
            "{}: success/running stay one row",
            case.name
        );
        // The Stage lays the same single row out: one tool row, no wrap, in both hosts.
        let legacy = legacy_conversation(call);
        assert_eq!(
            tool_rows(&local(&legacy, None, 80)).len(),
            1,
            "{}",
            case.name
        );
        assert_eq!(
            tool_rows(&attached(&legacy, None, &HashMap::new(), 80)).len(),
            1,
            "{}",
            case.name
        );
    }
}

// ── patch round (review 9-B P2, P3, P4, P9, P10) ───────────────────────────

#[test]
fn a_path_that_only_matches_the_root_after_cleaning_is_not_shown_relative() {
    let root = Path::new("/ws/proj");
    for (path, shown) in [
        // A control or bidi character inside the root segment makes a DIFFERENT
        // path: it shows in full (cleaned), never as `x.rs` under the workspace.
        ("/ws/proj\u{7}/x.rs", "/ws/proj/x.rs"),
        ("/ws/proj\u{202e}/x.rs", "/ws/proj/x.rs"),
        ("/ws/proj/src/x.rs", "src/x.rs"),
    ] {
        let call = info(
            "Read",
            json!({"file_path": path}),
            &ok(WriteDiffState::NotAWrite),
        );
        let row = header(&local(&legacy_conversation(call), Some(root), 80));
        assert_eq!(row, format!("┄ ✓ Read {shown} · 0.3s"), "{path:?}");
    }
    // A root with a double space matches the raw path (it used to never match).
    let spaced = Path::new("/ws/my  proj");
    let call = info(
        "Read",
        json!({"file_path": "/ws/my  proj/x.rs"}),
        &ok(WriteDiffState::NotAWrite),
    );
    assert_eq!(
        header(&local(&legacy_conversation(call), Some(spaced), 80)),
        "┄ ✓ Read x.rs · 0.3s"
    );
}

#[test]
fn an_annotation_only_write_diff_is_a_line_total_not_no_change() {
    // Bytes changed (a trailing newline) but no line did: the diff holds one
    // annotation row, so the row says how long the file is, never `no change`.
    let diff = WriteDiffState::from_original(b"a\n", "a");
    assert!(
        matches!(&diff, WriteDiffState::Diff { lines, more: 0 } if lines.len() == 1),
        "{diff:?}"
    );
    let call = info(
        "Write",
        json!({"file_path": "f.rs", "content": "a"}),
        &ok(diff),
    );
    assert_eq!(
        header(&local(&legacy_conversation(call), None, 80)),
        "┄ ✓ Write f.rs 1 line · 0.3s"
    );
}

#[test]
fn an_edit_too_large_to_count_exactly_shows_the_path_only() {
    let old: String = (0..600).map(|i| format!("old {i}\n")).collect();
    let new: String = (0..600).map(|i| format!("new {i}\n")).collect();
    let call = info(
        "Edit",
        json!({"file_path": "f.rs", "old_string": old, "new_string": new}),
        &ok(WriteDiffState::NotAWrite),
    );
    // Never the degraded `+600 −600`.
    assert_eq!(
        header(&local(&legacy_conversation(call), None, 80)),
        "┄ ✓ Edit f.rs · 0.3s"
    );
}

#[test]
fn calls_that_changed_nothing_show_no_change_summary() {
    let edit = json!({"file_path": "f.rs", "old_string": "a", "new_string": "b\nc"});
    let write = json!({"file_path": "f.rs", "content": "a\nb\nc"});
    for (status, glyph) in [
        (Some("⊘ Cancelled"), "⊘ "),
        (Some("⋯ Validating"), "⋯ "),
        (Some("⧖ Scheduled"), "⧖ "),
        (Some("? Awaiting approval"), "? "),
        (None, ""),
    ] {
        for (tool, input) in [("Edit", &edit), ("Write", &write)] {
            let mut call = info(tool, input.clone(), &None);
            call.status = status.map(str::to_owned);
            assert_eq!(
                header(&local(&legacy_conversation(call), None, 80)),
                format!("┄ {glyph}{tool} f.rs · 2.0s"),
                "{tool} {status:?}"
            );
        }
    }
    // A running call still shows its line total, a finished one its stat.
    let running = info("Write", write, &None);
    assert_eq!(
        header(&local(&legacy_conversation(running), None, 80)),
        "┄ ● Write f.rs 3 lines · 2.0s"
    );
}

#[test]
fn apply_patch_counts_distinct_paths() {
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Update File: src/a.rs\n@@\n-p\n+q\n*** Add File: src/b.rs\n+z\n*** End Patch\n";
    let call = info(
        "apply_patch",
        json!({ "patch": patch }),
        &ok(WriteDiffState::NotAWrite),
    );
    assert_eq!(
        header(&local(&legacy_conversation(call), None, 80)),
        "┄ ✓ apply_patch src/a.rs +1 file · 0.3s"
    );
}
