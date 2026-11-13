//! K5 cell escaping and K6 summary truncation (Story 21.12, AC3; DF-273,
//! DF-279; baselines B15, B16).
//!
//! The summary is produced by the real `PlanRuntime` (`on_turn_complete` →
//! `finish_plan` → `build_summary_markdown`) and painted through the chat
//! pane's PlanSummary branch; nothing here calls the builder directly.

use super::support::*;

use rustain::adapters::tui::theme::Theme;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    ChatMessage, ContentBlockType, Conversation, PermissionMode, Plan, PlanStatus, PlanTask,
    PlanTaskStatus, SubTaskFailurePolicy, TaskResult, generate_conversation_id,
};
use rustain::domain::ports::EventEmitter;
use rustain::domain::services::plan_runtime::{PlanRuntime, TaskTurnOutcome};

use crate::render_helpers::make_conversation;

const FAMILY: &str = "👨\u{200d}👩\u{200d}👧";

struct Sink;

impl EventEmitter for Sink {
    fn emit(&self, _event: AppEvent) {}
}

/// A task that already finished before the last one completes.
struct Done {
    title: &'static str,
    status: PlanTaskStatus,
    error: Option<&'static str>,
    result: Option<String>,
}

fn done(title: &'static str, result: &str) -> Done {
    Done {
        title,
        status: PlanTaskStatus::Completed,
        error: None,
        result: Some(result.to_owned()),
    }
}

fn task(number: u32, title: &str) -> PlanTask {
    PlanTask {
        number,
        title: title.to_owned(),
        description: String::new(),
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

/// Run a plan whose earlier tasks are `earlier` and whose last task completes
/// through `on_turn_complete`; return the PlanSummary message it commits.
async fn summary_message(earlier: Vec<Done>, last_title: &str, last_text: &str) -> ChatMessage {
    let mut tasks: Vec<PlanTask> = earlier
        .iter()
        .enumerate()
        .map(|(i, d)| task(u32::try_from(i).unwrap() + 1, d.title))
        .collect();
    let last = u32::try_from(tasks.len()).unwrap() + 1;
    tasks.push(task(last, last_title));
    let plan = Plan {
        id: "plan-k".to_owned(),
        title: "Plan".to_owned(),
        tasks,
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1_700_000_000,
        resolved_at: None,
        host_message_id: Some("host".to_owned()),
        origin_restrictions: None,
    };
    let mut conversation = Conversation {
        id: generate_conversation_id(),
        plans: [("plan-k".to_owned(), plan)].into(),
        ..make_conversation(vec![], vec![])
    };
    let conversation_id = conversation.id.clone();
    let runtime = PlanRuntime::new();
    runtime.clone().start(
        conversation_id.clone(),
        "plan-k".to_owned(),
        &mut conversation,
        &Sink,
        &[],
        PermissionMode::Normal,
        SubTaskFailurePolicy::default(),
    );
    {
        let plan = conversation.plans.get_mut("plan-k").unwrap();
        for (slot, d) in plan.tasks.iter_mut().zip(&earlier) {
            slot.status = d.status;
            slot.started_at_ms = Some(0);
            slot.completed_at_ms = Some(2_000);
            slot.error = d.error.map(str::to_owned);
            slot.result = d.result.clone().map(|text| TaskResult {
                text,
                tool_call_count: 0,
                token_count: None,
            });
        }
        let last_task = plan.tasks.last_mut().unwrap();
        last_task.status = PlanTaskStatus::Running;
        last_task.started_at_ms = Some(chrono::Utc::now().timestamp_millis());
    }
    runtime
        .on_turn_complete(
            &conversation_id,
            "plan-k",
            last,
            TaskTurnOutcome::Success {
                result_text: last_text.to_owned(),
                tool_call_count: 0,
                token_count: None,
            },
            &mut conversation,
            &Sink,
            &[],
            PermissionMode::Normal,
        )
        .await;
    conversation
        .messages
        .into_iter()
        .find(|m| m.content_blocks.contains(&ContentBlockType::PlanSummary))
        .expect("finish_plan commits a PlanSummary message")
}

fn paint_summary(message: ChatMessage, inner: u16) -> Vec<String> {
    let pane = paint_local(
        &make_conversation(vec![message], vec![]),
        None,
        &Default::default(),
        None,
        &Theme::dark(),
        inner + 2,
        80,
    );
    // A legacy message has no gutter: its rows start at column 0.
    rows(&pane.buffer)
}

/// The trimmed cells of a painted table row.
fn cells(row: &str) -> Vec<String> {
    let parts: Vec<&str> = row.split('│').collect();
    parts[1..parts.len().saturating_sub(1)]
        .iter()
        .map(|c| c.trim().to_owned())
        .collect()
}

#[tokio::test]
async fn k5_every_title_and_error_shows_literally_in_its_own_cell() {
    let titles = ["a | b", "x\\|y", "`t`", "*a*"];
    let mut earlier: Vec<Done> = titles.iter().map(|t| done(t, "fine")).collect();
    earlier.push(Done {
        title: "broken",
        status: PlanTaskStatus::Failed,
        error: Some("boom | pipe\nsecond line"),
        result: None,
    });
    let rows = paint_summary(summary_message(earlier, "last | one", "ok").await, 100);

    let header = rows
        .iter()
        .find(|r| r.contains("│ Task"))
        .unwrap_or_else(|| panic!("no table header in {rows:?}"));
    assert_eq!(cells(header), ["#", "Task", "Status", "Elapsed"]);
    for (number, title) in titles.iter().enumerate() {
        let row = rows
            .iter()
            .find(|r| r.starts_with(&format!("│ {} │", number + 1)))
            .unwrap_or_else(|| panic!("no row {} in {rows:?}", number + 1));
        let row_cells = cells(row);
        assert_eq!(row_cells.len(), 4, "{row}");
        assert_eq!(row_cells[1], *title);
        assert_eq!(row_cells[2], "✓");
    }
    let failed = rows.iter().find(|r| r.starts_with("│ 5 │")).unwrap();
    assert_eq!(&cells(failed)[1..3], ["broken", "✗ boom | pipe"]);
    let last = rows.iter().find(|r| r.starts_with("│ 6 │")).unwrap();
    assert_eq!(&cells(last)[1..3], ["last | one", "✓"]);
}

#[tokio::test]
async fn k6_a_result_cut_inside_a_fence_does_not_swallow_the_next_result() {
    let fenced = format!("```rust\n{}", "let x = 1;\n".repeat(60));
    let message = summary_message(
        vec![done("fence", &fenced), done("prose", "second result prose")],
        "last",
        "tail",
    )
    .await;
    assert!(message.content.contains("(truncated)"));
    let rows = paint_summary(message, 80);
    // The label and the result are one paragraph (a soft break joins them).
    assert!(
        rows.iter()
            .any(|r| r == "Task 2 result: second result prose"),
        "`Task 2 result:` must stay prose, got {rows:?}"
    );
    assert!(rows.iter().any(|r| r == "Task 3 result: tail"), "{rows:?}");

    // A SHORT result that already ends inside an open fence (a reply cut by the
    // model's token limit) must not swallow the next result either.
    let rows = paint_summary(
        summary_message(
            vec![
                done("short fence", "```rust\nlet x;"),
                done("prose", "second result prose"),
            ],
            "last",
            "tail",
        )
        .await,
        80,
    );
    assert!(
        rows.iter()
            .any(|r| r == "Task 2 result: second result prose"),
        "`Task 2 result:` must stay prose after a short open fence, got {rows:?}"
    );
    assert!(rows.iter().any(|r| r == "Task 3 result: tail"), "{rows:?}");
}

#[tokio::test]
async fn k6_a_cut_through_a_cluster_keeps_the_whole_cluster_or_drops_it() {
    let straddling = format!("{}{FAMILY}{}", "a".repeat(490), "z".repeat(60));
    assert!(straddling.len() > 500 && !straddling.is_char_boundary(499));
    let rows = paint_summary(
        summary_message(vec![done("c", &straddling)], "last", "x").await,
        600,
    );
    for row in &rows {
        if row.contains('👨') || row.contains('\u{200d}') {
            assert!(row.contains(FAMILY), "split cluster in {row:?}");
        }
    }
    let kept = rows
        .iter()
        .find(|r| r.starts_with("Task 1 result: aaaa"))
        .expect("the cut result paints");
    assert!(kept.ends_with("(truncated)"), "{kept:?}");
    assert!(
        kept.len() <= "Task 1 result: ".len() + 500 + " (truncated)".len(),
        "{} bytes",
        kept.len()
    );
    // Control: a cut that does not split a cluster keeps the text before it.
    let plain = format!("{}{}", "b".repeat(520), FAMILY);
    let rows = paint_summary(
        summary_message(vec![done("p", &plain)], "last", "x").await,
        600,
    );
    assert!(
        rows.iter()
            .any(|r| r.starts_with("Task 1 result: bbbb") && r.ends_with("(truncated)"))
    );
}
