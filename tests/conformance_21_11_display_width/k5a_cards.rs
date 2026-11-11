//! K5a — Story 21.11 AC5: dynamic cards and panels (plan card, ask-user
//! question, fork / rewind confirmation, Tasks tab, agent panel, adapter
//! status panel) render C/G/A text by display cells.
//!
//! Every test enters through the widget's public render entry, paints into a
//! real `Buffer` and reads the buffer back. Expected thresholds are written
//! out by hand from the documented geometry of each card (borders, indents,
//! prefixes, suffixes), never computed with the widget's own formula.
//!
//! Not covered here (private, unit-tested in the widget files instead):
//! the MCP sub-row builder of `adapter_status_panel`, because it is only
//! reachable through a feature-gated composite toolset adapter.

use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use rustain::adapters::tui::decision_queue::{self, RevertPreviewItem, RewindPreview};
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::mission_control::McTab;
use rustain::adapters::tui::presentation::reducer;
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::ask_user_question::question_lines;
use rustain::adapters::tui::widgets::plan_card::render_plan_card_lines;
use rustain::adapters::tui::widgets::{adapter_status_panel, mission_control};
use rustain::domain::models::plan::{
    DelegationInfo, Plan, PlanStatus, PlanSubTask, PlanTask, PlanTaskStatus,
};
use rustain::domain::models::profile::{AdapterRef, PortDimension};
use rustain::domain::models::{AgentId, AgentRowView, HealthSummary, NodeState, OwnershipKind};
use rustain::infrastructure::runtime::mission_bridge;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::support::*;

/// (unit, cells per unit): wide CJK, ZWJ family, combining accent, ASCII control.
const UNITS: [(&str, usize); 4] = [(C, 2), (G, 2), (A, 1), ("a", 1)];

// ───────────────────────── shared observers ─────────────────────────

fn paint_lines(lines: Vec<Line<'static>>, width: u16) -> Buffer {
    let height = u16::try_from(lines.len()).expect("card height fits u16");
    paint(width, height, |f| {
        let area = f.area();
        f.render_widget(Paragraph::new(lines), area);
    })
    .buffer
}

/// Row text between the first and last glyph, edge blanks trimmed.
fn interior(buffer: &Buffer, y: u16) -> String {
    let g = glyphs(buffer, y);
    g[1..g.len() - 1].concat().trim().to_string()
}

/// Every row that opens with `vertical` closes with it in the last column,
/// and no row holds a partial grapheme cluster.
fn assert_right_border(buffer: &Buffer, vertical: &str) {
    let width = usize::from(buffer.area.width);
    for y in buffer.area.top()..buffer.area.bottom() {
        let g = glyphs(buffer, y);
        if g.first().map(String::as_str) != Some(vertical) {
            continue;
        }
        assert_eq!(
            g.last().map(String::as_str),
            Some(vertical),
            "row {y} lost its right border: {:?}",
            row_text(buffer, y)
        );
        assert_eq!(
            row_cells(buffer, y),
            width,
            "row {y} is not {width} cells wide"
        );
    }
    assert_whole_clusters(buffer);
}

// ───────────────────────── plan card ─────────────────────────

fn plan_task(number: u32, title: &str, description: &str) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: description.to_string(),
        depends_on: vec![],
        status: PlanTaskStatus::Pending,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
        delegated_to: None,
        sub_tasks: vec![],
    }
}

fn plan_sub_task(number: u32, title: &str) -> PlanSubTask {
    PlanSubTask {
        number,
        title: title.to_string(),
        description: String::new(),
        status: PlanTaskStatus::Pending,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        delegated_to: None,
    }
}

fn plan_of(title: &str, tasks: Vec<PlanTask>) -> Plan {
    Plan {
        id: "k5a".to_string(),
        title: title.to_string(),
        tasks,
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 0,
        resolved_at: None,
        host_message_id: None,
    }
}

fn plan_buffer(plan: &Plan, width: u16) -> Buffer {
    let lines = render_plan_card_lines(plan, &Theme::dark(), width, false);
    paint_lines(lines, width)
}

fn plan_border(width: u16) -> &'static str {
    if width < 64 { "│" } else { "║" }
}

#[test]
fn k5a_plan_card_real_width_panic_vectors_render_complete_and_bounded() {
    // Baseline: header byte 71, task byte 52 and wrapper byte 74 were not
    // UTF-8 boundaries and panicked.
    let title = C.repeat(25);
    let buf = plan_buffer(&plan_of(&title, vec![plan_task(1, "Step", "")]), 80);
    assert_right_border(&buf, plan_border(80));
    assert_eq!(interior(&buf, 1), format!("Plan: {title}"));

    let task_title = C.repeat(18);
    let buf = plan_buffer(&plan_of("P", vec![plan_task(1, &task_title, "")]), 60);
    assert_right_border(&buf, plan_border(60));
    assert_eq!(interior(&buf, 3), format!("1. {task_title}"));

    let description = C.repeat(25);
    let buf = plan_buffer(&plan_of("P", vec![plan_task(1, "Step", &description)]), 80);
    assert_right_border(&buf, plan_border(80));
    assert_eq!(interior(&buf, 4), description);
}

#[test]
fn k5a_plan_card_header_budget_at_60_80_130() {
    // ` Plan: <title> ` must fit the inner width: titles up to inner - 8 cells.
    for (width, budget) in [(60u16, 50usize), (80, 70), (130, 120)] {
        for (unit, cells) in UNITS {
            let exact = unit.repeat(budget / cells);
            let buf = plan_buffer(&plan_of(&exact, vec![plan_task(1, "S", "")]), width);
            assert_right_border(&buf, plan_border(width));
            assert_eq!(interior(&buf, 1), format!("Plan: {exact}"), "width {width}");

            let (over, kept) = if cells == 2 {
                (unit.repeat(budget / 2 + 1), unit.repeat(budget / 2))
            } else {
                (unit.repeat(budget + 3), unit.repeat(budget + 1))
            };
            let buf = plan_buffer(&plan_of(&over, vec![plan_task(1, "S", "")]), width);
            assert_right_border(&buf, plan_border(width));
            assert_eq!(interior(&buf, 1), format!("{kept}..."), "width {width}");
        }
    }
}

#[test]
fn k5a_plan_card_task_and_sub_task_titles_fit_the_row() {
    // (width, [(unit, first title length in units that must be truncated)]).
    // Task row: two-cell indent + `1. ` + title + marker inside inner - 2.
    let task_table: [(u16, [usize; 4]); 3] = [
        (60, [27, 27, 54, 54]),
        (80, [37, 37, 74, 74]),
        (130, [62, 62, 124, 124]),
    ];
    // Sub-task row: `    1. ` (7 cells) + title + marker inside inner - 7.
    let sub_table: [(u16, [usize; 4]); 3] = [
        (60, [26, 26, 52, 52]),
        (80, [36, 36, 72, 72]),
        (130, [61, 61, 122, 122]),
    ];
    for (width, firsts) in task_table {
        for ((unit, _), first) in UNITS.into_iter().zip(firsts) {
            for k in [first - 1, first] {
                let title = unit.repeat(k);
                let buf = plan_buffer(&plan_of("P", vec![plan_task(1, &title, "")]), width);
                assert_right_border(&buf, plan_border(width));
                let row = interior(&buf, 3);
                if k < first {
                    assert_eq!(row, format!("1. {title}"), "width {width}");
                } else {
                    assert!(row.ends_with("...") && !row.contains(&title), "{row:?}");
                }
            }
        }
    }
    for (width, firsts) in sub_table {
        for ((unit, _), first) in UNITS.into_iter().zip(firsts) {
            for k in [first - 1, first] {
                let title = unit.repeat(k);
                let mut task = plan_task(1, "Parent", "");
                task.sub_tasks = vec![plan_sub_task(1, &title)];
                let buf = plan_buffer(&plan_of("P", vec![task]), width);
                assert_right_border(&buf, plan_border(width));
                let row = interior(&buf, 4);
                if k < first {
                    assert_eq!(row, format!("1. {title}"), "width {width}");
                } else {
                    assert!(row.ends_with("...") && !row.contains(&title), "{row:?}");
                }
            }
        }
    }
}

#[test]
fn k5a_plan_card_descriptions_wrap_by_cells_without_loss() {
    // Width 20: task description budget 14 cells, sub-task description 10.
    for (word, rows) in [
        (C.repeat(25), vec![7usize, 7, 7, 4]),
        (G.repeat(10), vec![7, 3]),
        (A.repeat(30), vec![14, 14, 2]),
    ] {
        let buf = plan_buffer(&plan_of("T", vec![plan_task(1, "S", &word)]), 20);
        assert_right_border(&buf, plan_border(20));
        let last = buf.area.bottom() - 3;
        let got: Vec<String> = (4..=last).map(|y| interior(&buf, y)).collect();
        assert_eq!(got.len(), rows.len(), "{got:?}");
        for (row, expected) in got.iter().zip(&rows) {
            assert_eq!(row.graphemes(true).count(), *expected, "{got:?}");
        }
        assert_eq!(got.concat(), word, "no source text lost or duplicated");
    }
}

#[test]
fn k5a_plan_card_icon_and_badge_rows_keep_the_right_border() {
    // Short titles keep their badges whole; long ASCII and C×40 titles on
    // started / completed / failed / waiting / running / delegated rows and on
    // sub-task rows are ellipsized so icon, elapsed and badge still fit and the
    // right border stays in the last column of every card row.
    let started = Some(1_700_000_000_000);
    let finished = Some(1_700_000_005_000);
    for width in [60u16, 80, 130] {
        for name in [
            C.repeat(10),
            G.repeat(4),
            A.repeat(9),
            "reviewer".to_string(),
        ] {
            for title in [
                "Init".to_string(),
                "x".repeat(100),
                C.repeat(40),
                format!("{G}{A}{}", "x".repeat(90)),
            ] {
                let long = title != "Init";
                let info = DelegationInfo {
                    agent_name: name.clone(),
                    agent_id: None,
                    delegated_at_ms: 1,
                    spool_task_id: None,
                };
                let mut done = plan_task(1, &title, "");
                done.status = PlanTaskStatus::Completed;
                done.started_at_ms = started;
                done.completed_at_ms = finished;
                done.delegated_to = Some(info.clone());
                let mut failed = plan_task(2, &title, "");
                failed.status = PlanTaskStatus::Failed;
                failed.started_at_ms = started;
                failed.completed_at_ms = finished;
                let mut skipped = plan_task(3, &title, "");
                skipped.status = PlanTaskStatus::Skipped;
                skipped.started_at_ms = started;
                skipped.completed_at_ms = finished;
                let mut waiting = plan_task(4, &title, "");
                waiting.status = PlanTaskStatus::Waiting;
                waiting.waiting_on = vec![1, 2];
                waiting.started_at_ms = started;
                waiting.completed_at_ms = finished;
                let mut running = plan_task(5, &title, "");
                running.status = PlanTaskStatus::Running;
                running.started_at_ms = started;
                let mut parent = plan_task(6, "Parent", "");
                let mut child = plan_sub_task(1, &title);
                child.status = PlanTaskStatus::Completed;
                child.started_at_ms = started;
                child.completed_at_ms = finished;
                child.delegated_to = Some(info);
                let mut child_failed = plan_sub_task(2, &title);
                child_failed.status = PlanTaskStatus::Failed;
                child_failed.started_at_ms = started;
                child_failed.completed_at_ms = finished;
                parent.sub_tasks = vec![child, child_failed];
                let plan = plan_of(
                    "Mixed",
                    vec![done, failed, skipped, waiting, running, parent],
                );
                let buf = plan_buffer(&plan, width);
                assert_right_border(&buf, plan_border(width));
                let text = screen(&buf);
                assert_eq!(
                    text.matches(&format!("[delegated → {name}]")).count(),
                    2,
                    "task and sub-task badges whole at {width}: {text}"
                );
                // Each of the 7 titled rows (5 tasks + 2 sub-tasks) shows its
                // title whole or ends it with one "..." — never neither.
                assert_eq!(
                    text.matches(title.as_str()).count() + text.matches("...").count(),
                    7,
                    "{text}"
                );
                if long && width == 60 {
                    // 58 inner cells cannot hold these titles beside any suffix.
                    assert_eq!(text.matches("...").count(), 7, "{text}");
                }
            }
        }
    }
}

// ───────────────────────── ask-user question ─────────────────────────
//
// 21-6 6-C: the question is the body of a `Decision: Question` frame. The body builder
// (`question_lines`) wraps by display cells; the frame owns the border, so the old
// "right border" witnesses are re-pointed to the builder rows and to the painted frame.

/// Row text of a border-less builder row, trailing blanks trimmed.
fn plain_row(buffer: &Buffer, y: u16) -> String {
    glyphs(buffer, y).concat().trim_end().to_string()
}

fn question_buffer(question: &str, width: usize) -> Buffer {
    let lines = question_lines(question, width, &Theme::dark());
    paint_lines(lines, u16::try_from(width).expect("builder width fits u16"))
}

#[test]
fn k5a_ask_question_is_fully_wrapped_never_clipped() {
    // Width 26: C×20 (40 cells) must wrap, its tail joins A×10.
    let question = format!("{} {}", C.repeat(20), A.repeat(10));
    let buf = question_buffer(&question, 26);
    assert_eq!(plain_row(&buf, 0), C.repeat(13));
    assert_eq!(
        plain_row(&buf, 1),
        format!("{} {}", C.repeat(7), A.repeat(10))
    );
    // ASCII control: a short question is one untouched row.
    let buf = question_buffer("What is the name of your project?", 56);
    assert_eq!(buf.area.height, 1);
    assert_eq!(plain_row(&buf, 0), "What is the name of your project?");

    // Real inner widths 56/76/126: exact fit stays one row, one over wraps.
    for inner in [56usize, 76, 126] {
        for (unit, cells) in UNITS {
            let exact = unit.repeat(inner / cells);
            let buf = question_buffer(&exact, inner);
            assert_eq!(buf.area.height, 1, "{unit:?} at {inner}");
            assert_eq!(plain_row(&buf, 0), exact);

            let buf = question_buffer(&unit.repeat(inner / cells + 1), inner);
            assert_eq!(buf.area.height, 2, "{unit:?} at {inner}");
            assert_eq!(plain_row(&buf, 0), exact);
            assert_eq!(plain_row(&buf, 1), unit);
        }
    }
}

/// The painted frame's body rows, borders stripped, concatenated: a wrapped unbroken
/// question reads back whole.
fn frame_body(buffer: &Buffer) -> String {
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| {
            let row = plain_row(buffer, y);
            // At >= 100 columns the frame is centered: only what sits between its borders.
            match (row.find('│'), row.rfind('│')) {
                (Some(open), Some(close)) if close > open => {
                    row[open + '│'.len_utf8()..close].trim().to_string()
                }
                _ => String::new(),
            }
        })
        .collect::<String>()
}

/// Frame width at `terminal_width` (mirrors `presentation::geometry::frame_rect`).
fn frame_cells(terminal_width: u16) -> usize {
    if terminal_width < 100 {
        usize::from(terminal_width)
    } else {
        (usize::from(terminal_width) * 9 / 10).max(1)
    }
}

/// Cells from the first to the last `│` of row `y`, both included (0 when it has none).
fn frame_span(buffer: &Buffer, y: u16) -> usize {
    let cells = glyphs(buffer, y);
    match (
        cells.iter().position(|glyph| glyph == "│"),
        cells.iter().rposition(|glyph| glyph == "│"),
    ) {
        (Some(open), Some(close)) => cells[open..=close]
            .iter()
            .map(|glyph| UnicodeWidthStr::width(glyph.as_str()))
            .sum(),
        _ => 0,
    }
}

#[test]
fn k5a_ask_question_in_the_frame_is_complete_by_cells_at_60_80_130() {
    use crate::conformance_21_6_decision_queue::support::{clocked, facts, paint as paint_frame};
    for width in [60u16, 80, 130] {
        for (unit, cells) in UNITS {
            let question = unit.repeat(usize::from(width) * 2 / cells);
            let (mut state, _clock) = clocked(width, 24);
            let (tx, _rx) = tokio::sync::oneshot::channel();
            rustain::adapters::tui::decision_queue::raise_question(
                &mut state,
                "c1",
                "k5a".into(),
                question.clone(),
                tx,
            );
            let buffer = paint_frame(&mut state, &facts("c1"), width, 24);
            assert!(
                frame_body(&buffer).contains(&question),
                "{unit:?} at {width}: the whole question is painted"
            );
        }
    }
}

#[test]
fn k5a_ask_editable_suffix_and_caret_stay_visible_with_answer_bytes_intact() {
    use crate::conformance_21_6_decision_queue::support::{
        clocked, facts, paint as paint_frame, row_of, row_text,
    };
    use rustain::adapters::tui::app::handle_input;
    use rustain::adapters::tui::decision_queue::DecisionSubject;
    use rustain::domain::events::DomainInputEvent;
    for width in [60u16, 80, 130] {
        for (unit, cells) in UNITS {
            let long = format!("HEAD{}", unit.repeat(usize::from(width) / cells + 3));
            let (mut state, _clock) = clocked(width, 24);
            let (tx, _rx) = tokio::sync::oneshot::channel();
            rustain::adapters::tui::decision_queue::raise_question(
                &mut state,
                "c1",
                "k5a".into(),
                "Q?".into(),
                tx,
            );
            handle_input(&mut state, &DomainInputEvent::Paste(long.clone()));
            let buffer = paint_frame(&mut state, &facts("c1"), width, 24);
            let at = row_of(&buffer, "Answer: ").expect("the answer row");
            let row = row_text(&buffer, at);
            // The editable end stays visible: HEAD scrolled off, whole units remain, then
            // the caret, and the row keeps its right border in the last column.
            assert!(!row.contains("HEAD"), "{unit:?} at {width}: {row}");
            assert!(row.contains(unit), "{unit:?} at {width}: {row}");
            assert!(
                row.contains('▋'),
                "{unit:?} at {width}: the caret is visible: {row}"
            );
            // Border to border the row is exactly the frame (a full sheet below 100
            // columns, 90 % centered above): the right border closes the answer row.
            assert_eq!(
                frame_span(&buffer, at),
                frame_cells(width),
                "{unit:?} at {width}"
            );
            assert!(UnicodeWidthStr::width(row.as_str()) <= usize::from(width));
            // The answer bytes are untouched (only the view clips).
            let stored = state
                .decisions
                .items()
                .find_map(|item| match &item.subject {
                    DecisionSubject::Question(payload) => Some(payload.field.as_str().to_owned()),
                    _ => None,
                });
            assert_eq!(
                stored.as_deref(),
                Some(long.as_str()),
                "answer bytes untouched"
            );
        }
    }
}

#[test]
fn k5a_ask_mixed_answer_places_the_right_border() {
    use crate::conformance_21_6_decision_queue::support::{
        clocked, facts, paint as paint_frame, row_of, row_text,
    };
    use rustain::adapters::tui::app::handle_input;
    use rustain::domain::events::DomainInputEvent;
    let typed = format!("{G}{C}");
    for width in [60u16, 80, 130] {
        let (mut state, _clock) = clocked(width, 24);
        let (tx, _rx) = tokio::sync::oneshot::channel();
        rustain::adapters::tui::decision_queue::raise_question(
            &mut state,
            "c1",
            "k5a".into(),
            "Q?".into(),
            tx,
        );
        handle_input(&mut state, &DomainInputEvent::Paste(typed.clone()));
        let buffer = paint_frame(&mut state, &facts("c1"), width, 24);
        let at = row_of(&buffer, "Answer: ").expect("the answer row");
        let row = row_text(&buffer, at);
        assert!(row.contains(&format!("Answer: {typed}")), "{width}: {row}");
        assert_eq!(
            frame_span(&buffer, at),
            frame_cells(width),
            "{width}: the right border closes the frame"
        );
        assert!(
            UnicodeWidthStr::width(row.as_str()) <= usize::from(width),
            "{width}"
        );
    }
}

// ───────────────────────── fork confirmation ─────────────────────────

fn fork_buffer(preview: &str, index: usize, width: u16) -> Buffer {
    super::k5b_cards::paint_decision(width, "c1", |state| {
        decision_queue::raise_fork(state, "c1", index, preview, 0);
    })
}

#[test]
fn k5a_fork_preview_budget_exact_fit_and_over() {
    // The borderless builder caps previews at 50 cells; the frame wraps the
    // resulting row instead of reducing the cap at narrower terminal widths.
    for width in [60u16, 80, 130] {
        for (unit, cells) in UNITS {
            let exact = unit.repeat(50 / cells);
            let buf = fork_buffer(&exact, 0, width);
            let body = super::k5b_cards::compact(&buf);
            assert!(
                body.contains(&format!("Message1:\"{exact}\"")),
                "{width}: {body}"
            );
            let over = unit.repeat(50 / cells + 1);
            let buf = fork_buffer(&over, 0, width);
            let body = super::k5b_cards::compact(&buf);
            let kept = unit.repeat(49 / cells);
            assert!(
                body.contains(&format!("Message1:\"{kept}…\"")),
                "{width}: {body}"
            );
            assert!(body.contains("Fork") && body.contains("Cancel"), "{body}");
            assert!(
                body.contains("Anewtabwillbecreatedwithmessagesuptothispoint."),
                "{body}"
            );
        }
        for (input, expected) in [
            (
                format!("{}{G}xx", "x".repeat(49)),
                format!("{}…", "x".repeat(49)),
            ),
            // A two-cell grapheme that would end at cell 50 does not fit beside the marker.
            (
                format!("{}{G}xx", "x".repeat(48)),
                format!("{}…", "x".repeat(48)),
            ),
            ("a".repeat(200), format!("{}…", "a".repeat(49))),
        ] {
            let buf = fork_buffer(&input, 8, width);
            let body = super::k5b_cards::compact(&buf);
            assert!(
                body.contains(&format!("Message9:\"{expected}\"")),
                "{width}: {body}"
            );
        }
    }
}

fn rewind_buffer(path: &str, conflict: bool, width: u16) -> Buffer {
    let preview = RewindPreview {
        target_message_index: 2,
        messages_to_remove: 1,
        files_to_revert: vec![RevertPreviewItem {
            display_path: path.to_string(),
            conflict,
        }],
    };
    super::k5b_cards::paint_decision(width, "c1", |state| {
        decision_queue::raise_rewind(state, "c1", preview, 0);
    })
}

#[test]
fn k5a_rewind_path_budget_exact_fit_and_over_at_60_80_130() {
    // The builder caps a file row at 53 cells, including marker and suffix.
    for width in [60u16, 80, 130] {
        for (unit, cells) in UNITS {
            for (conflict, budget, prefix, suffix) in [
                (false, 51usize, "• ", ""),
                (true, 29usize, "⚠ ", " (modified externally)"),
            ] {
                let exact = unit.repeat(budget / cells);
                let buf = rewind_buffer(&exact, conflict, width);
                let body = super::k5b_cards::compact(&buf);
                let expected: String = format!("{prefix}{exact}{suffix}")
                    .split_whitespace()
                    .collect();
                assert!(body.contains(&expected), "{width}: {body}");
                let over = unit.repeat(budget / cells + 1);
                let buf = rewind_buffer(&over, conflict, width);
                let body = super::k5b_cards::compact(&buf);
                let kept = unit.repeat((budget - 1) / cells);
                let expected: String = format!("{prefix}{kept}…{suffix}")
                    .split_whitespace()
                    .collect();
                assert!(body.contains(&expected), "{width}: {body}");
                assert!(
                    body.contains("Rewind")
                        && body.contains("Forkinstead")
                        && body.contains("Cancel"),
                    "{body}"
                );
                if conflict {
                    assert!(body.contains("Allfilesmodifiedexternally"), "{body}");
                }
            }
        }
        for (path, expected) in [
            (
                format!("{}{G}xx", "x".repeat(49)),
                format!("•{}…", "x".repeat(49)),
            ),
            (
                format!("{}{G}xx", "x".repeat(48)),
                format!("•{}{G}…", "x".repeat(48)),
            ),
            (C.repeat(30), format!("•{}…", C.repeat(25))),
        ] {
            let body = super::k5b_cards::compact(&rewind_buffer(&path, false, width));
            assert!(body.contains(&expected), "{width}: {body}");
        }
    }
}

// ───────────────────────── Tasks tab ─────────────────────────

fn delegated_plan(agent_name: &str, on_sub_task: bool) -> Plan {
    let info = DelegationInfo {
        agent_name: agent_name.to_string(),
        agent_id: None,
        delegated_at_ms: 1_700_000_000,
        spool_task_id: None,
    };
    let mut task = plan_task(1, "Ship", "");
    if on_sub_task {
        let mut sub = plan_sub_task(1, "Sub");
        sub.delegated_to = Some(info);
        task.sub_tasks = vec![sub];
    } else {
        task.delegated_to = Some(info);
    }
    let mut plan = plan_of("Panel", vec![task]);
    plan.status = PlanStatus::Executing;
    plan
}

/// The list row of the delegated task (or sub-task) painted by the real hub renderer on the
/// Tasks tab at `width` x 24 (the Tasks sidebar panel is gone, 21-7 7-D): the first painted row
/// that carries the marker, cut to the list column (written out by hand: from an inner width of 96
/// the hub splits master / detail, master = 40% of the inner width, at least 28, else the list
/// owns the whole inner width) and stripped of its 2-cell marker and trailing blanks.
fn task_row(name: &str, on_sub_task: bool, width: u16) -> (Buffer, String) {
    let plan = delegated_plan(name, on_sub_task);
    let mut state = TuiState::new(width, 24);
    let conversation = rustain::domain::models::Conversation {
        id: "k5a-conv".to_string(),
        plans: std::collections::HashMap::from([(plan.id.clone(), plan.clone())]),
        ..rustain::domain::models::Conversation::default()
    };
    state.task_panel_state.last_executed_plan_id = Some(plan.id.clone());
    let _ = reducer::open_mission_control(&mut state, McTab::Tasks);
    rustain::adapters::tui::mission_control::tasks::sync(&mut state, &conversation);
    let buf = paint(width, 24, |f| {
        let area = f.area();
        mission_control::render(f, area, &state, "Mission Control · Tasks", Host::Local);
    })
    .buffer;
    let inner = usize::from(width) - 2;
    let master = if inner >= 96 {
        (inner * 40 / 100).max(28)
    } else {
        inner
    };
    for y in buf.area.top()..buf.area.bottom() {
        let mut used = 0;
        let mut column = String::new();
        for g in glyphs(&buf, y).iter().skip(1) {
            let cells = g.width().max(1);
            if used + cells > master {
                break;
            }
            used += cells;
            column.push_str(g);
        }
        if column.contains("[delegated") {
            return (buf, column.trim_end().to_string());
        }
    }
    panic!("no painted Tasks row carries the delegation marker at width {width}");
}

#[test]
fn k5a_task_row_delegated_name_marker_is_inside_the_12_cell_budget() {
    for width in [60u16, 80, 130] {
        for on_sub_task in [false, true] {
            let render_row = |name: &str| {
                let (buf, row) = task_row(name, on_sub_task, width);
                assert_whole_clusters(&buf);
                row
            };
            for (unit, cells) in UNITS {
                let exact = unit.repeat(12 / cells);
                let row = render_row(&exact);
                assert!(row.contains(&format!("[delegated → {exact}]")), "{row:?}");
                let over = unit.repeat(12 / cells + 1);
                let kept = unit.repeat(11 / cells);
                let row = render_row(&over);
                assert!(row.contains(&format!("[delegated → {kept}…]")), "{row:?}");
            }
            // ASCII control: a 13-char name keeps 11 chars + the marker.
            let row = render_row("code-reviewer");
            assert!(row.contains("[delegated → code-review…]"), "{row:?}");
            let row = render_row("reviewer");
            assert!(row.contains("[delegated → reviewer]"), "{row:?}");
        }
    }
}

// ───────────────────────── agent panel ─────────────────────────

fn agent_entry(status: NodeState, isolated: bool) -> AgentRowView {
    AgentRowView {
        isolated,
        agent_id: AgentId::new(),
        parent_id: AgentId::root(),
        subagent_type: "模型".to_string(),
        spawned_at: 0,
        depth: 1,
        current_status: status,
        ownership: OwnershipKind::Owned,
        effective_model: String::new(),
        tools_summary: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        turns: 0,
        wait_reason: None,
    }
}

/// The first list row of the Agents tab painted by the real hub renderer at
/// `width` x 24 (inner row 0 is the tab row, row 1 the first list row), cut
/// to the list column and stripped of its 2-cell marker and trailing blanks.
/// Returns the buffer too, for the cluster and border checks.
fn agent_row(entry: &AgentRowView, tail: &str, width: u16) -> (Buffer, String) {
    let mut state = TuiState::new(width, 24);
    state
        .agent_panel_state
        .spool_tail_cache
        .insert(entry.agent_id.clone(), tail.to_string());
    mission_bridge::apply_roster(&mut state, vec![entry.clone()]);
    let _ = reducer::open_mission_control(&mut state, McTab::Agents);
    let buf = paint(width, 24, |f| {
        let area = f.area();
        mission_control::render(f, area, &state, "Mission Control · Agents", Host::Local);
    })
    .buffer;
    // Written out by hand: from an inner width of 96 the hub splits master /
    // detail (master = 40% of the inner width, at least 28), else the list
    // owns the whole inner width. The row is cut 2 cells short of its column.
    let inner = usize::from(width) - 2;
    let master = if inner >= 96 {
        (inner * 40 / 100).max(28)
    } else {
        inner
    };
    let mut used = 0;
    let mut column = String::new();
    for g in glyphs(&buf, 2).iter().skip(1) {
        let cells = g.width().max(1);
        if used + cells > master {
            break;
        }
        used += cells;
        column.push_str(g);
    }
    let text = column
        .strip_prefix("▸ ")
        .or_else(|| column.strip_prefix("  "))
        .unwrap_or_else(|| panic!("row without a 2-cell marker: {column:?}"))
        .trim_end()
        .to_string();
    (buf, text)
}

#[test]
fn k5a_agent_row_tail_never_splits_a_cluster_or_pushes_out_the_status() {
    let tails = [
        C.repeat(30),
        G.repeat(25),
        A.repeat(60),
        format!("{G}{A}{}", C.repeat(20)),
        "a".repeat(60),
    ];
    // The row is `<state> <owner> <type>[ ⊙ iso] <status>[  <tail>]`: the head
    // (through the status word) comes before the tail, so a cut hits the tail.
    for (status, isolated, head) in [
        (NodeState::Running, false, "▶ ♦ 模型 running"),
        (NodeState::Created, false, "○ ♦ 模型 queued"),
        (NodeState::Running, true, "▶ ♦ 模型 ⊙ iso running"),
        (NodeState::Created, true, "○ ♦ 模型 ⊙ iso queued"),
    ] {
        let entry = agent_entry(status, isolated);
        let status_word = head.rsplit(' ').next().unwrap();
        // The narrowest width (28: list column 26, row room 24) holds the head.
        assert!(head.width() <= 24, "{head:?}");
        for tail in &tails {
            let full = format!("{head} {tail}");
            for width in 28u16..=130 {
                let (buf, text) = agent_row(&entry, tail, width);
                let context = format!("width {width} {status:?} iso={isolated} {text:?}");
                assert_whole_clusters(&buf);
                assert_eq!(glyphs(&buf, 2).last().map(String::as_str), Some("│"));
                // Head first, status word never pushed out.
                assert!(text.starts_with(head), "{context}");
                assert!(text.contains(status_word), "{context}");
                // The painted row is a whole-grapheme prefix of head + tail
                // that fills the row room (a cut may drop at most one wide
                // cluster's worth of cells).
                assert!(full.starts_with(&text), "{context}");
                let inner = usize::from(width) - 2;
                let master = if inner >= 96 {
                    (inner * 40 / 100).max(28)
                } else {
                    inner
                };
                let room = master - 2;
                assert!(text.width() <= room, "{context}");
                if text != full {
                    // One separator and one wide cluster may not fit.
                    assert!(text.width() + 3 > room, "{context}");
                }
            }
        }
    }
    // ASCII positive controls.
    let entry = agent_entry(NodeState::Running, false);
    let (_, text) = agent_row(&entry, "build the project", 80);
    assert_eq!(text, "▶ ♦ 模型 running build the project");
    // Width 60: list room 56, head + gap 18 cells, 38 cells of tail.
    let (_, text) = agent_row(&entry, &"x".repeat(100), 60);
    assert_eq!(text, format!("▶ ♦ 模型 running {}", "x".repeat(39)));
}

// ───────────────────────── adapter status panel ─────────────────────────

/// Row y of the scheduler port in the painted lines: persona, memory,
/// session, tools, channels above it (no border, no title row).
const SCHEDULER_Y: u16 = 5;

/// The adapter lines laid out at the frame inner width (`width - 2`) and
/// painted one line per row into a buffer exactly that wide.
fn adapter_buffer(width: u16, metric: Option<&str>, name: Option<&str>) -> Buffer {
    let inner = width - 2;
    let mut overrides = BTreeMap::new();
    if let Some(name) = name {
        overrides.insert(
            PortDimension::Scheduler,
            AdapterRef {
                adapter: name.to_string(),
                _config: None,
            },
        );
    }
    let rows = adapter_status_panel::build(
        |port| match (port, metric) {
            (PortDimension::Scheduler, Some(metric)) => HealthSummary::healthy(metric.to_string()),
            _ => HealthSummary::unknown(),
        },
        Vec::new(),
        None,
        &overrides,
    );
    let lines = adapter_status_panel::lines(&rows, inner, &Theme::dark());
    let mut buf = Buffer::empty(Rect::new(0, 0, inner, 14));
    for (y, line) in lines.iter().enumerate() {
        buf.set_line(0, y as u16, line, inner);
    }
    buf
}

#[test]
fn k5a_adapter_panel_name_budget_and_metric_alignment() {
    // Adapter name budget = min(24, inner - 40): 18 / 24 / 24 cells.
    for (width, budget) in [(60u16, 18usize), (80, 24), (130, 24)] {
        for (unit, cells) in UNITS {
            let exact = unit.repeat(budget / cells);
            let buf = adapter_buffer(width, None, Some(exact.as_str()));
            assert_whole_clusters(&buf);
            let row = row_text(&buf, SCHEDULER_Y);
            assert!(row.contains(&format!(": {exact} ")), "{row:?}");

            let over = unit.repeat(budget / cells + 1);
            let buf = adapter_buffer(width, None, Some(over.as_str()));
            let row = row_text(&buf, SCHEDULER_Y);
            let kept = unit.repeat((budget - 1) / cells);
            assert!(row.contains(&format!(": {kept}…")), "{row:?}");
        }
        // The right metric ends at inner.width - 3 whatever the name and
        // metric widths: `A + " ready"` used to shift it.
        for name in ["cron".to_string(), format!("{C}{G}"), format!("{G}{A}{C}")] {
            for metric in [
                "e ready".to_string(),
                format!("{A} ready"),
                format!("{C} ready"),
            ] {
                let buf = adapter_buffer(width, Some(metric.as_str()), Some(name.as_str()));
                let text = row_text(&buf, SCHEDULER_Y);
                assert!(text.contains(&metric), "{text:?}");
                // The metric's last cell must sit just before the inner
                // area's three-cell right margin: cell `inner - 3` (== `width - 5`),
                // exclusive. The buffer is the inner area itself: no border.
                let g = glyphs(&buf, SCHEDULER_Y);
                let mut body: Vec<&String> = g.iter().collect();
                while body.last().is_some_and(|s| s.trim().is_empty()) {
                    body.pop();
                }
                let metric_end: usize = body.iter().map(|s| s.width().max(1)).sum();
                assert_eq!(
                    metric_end,
                    usize::from(width) - 5,
                    "metric {metric:?} name {name:?} width {width}"
                );
            }
        }
    }
    // Zero budget (inner 30): the name is dropped and no marker appears.
    let name = format!("{G}{C}{A}");
    let buf = adapter_buffer(32, None, Some(name.as_str()));
    assert_whole_clusters(&buf);
    let row = row_text(&buf, SCHEDULER_Y);
    assert!(
        !row.contains('…') && !row.contains(C) && !row.contains('👨'),
        "{row:?}"
    );
    assert!(row.contains("scheduler"), "{row:?}");
    // ASCII control: 28 chars over a 24-cell budget → 23 chars + "…".
    let buf = adapter_buffer(80, None, Some("a-very-long-adapter-name-xyz"));
    let row = row_text(&buf, SCHEDULER_Y);
    assert!(row.contains(": a-very-long-adapter-nam…"), "{row:?}");
}
