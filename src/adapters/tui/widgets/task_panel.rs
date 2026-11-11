//! The Tasks tab's row builders (Story 21.7 A6; Story 6.3's sidebar panel became a Mission
//! Control tab).
//!
//! One builder produces every row text — the hub list, its wide detail preview and the task
//! window's header all read these functions — so what the list can show is what the keys can
//! reach (height == paint; DF-CR-10-6-2: the old panel built multi-line list items whose
//! painted height exceeded the item count). A task is one row; each of its sub-tasks is one
//! more row directly beneath it (never a hidden line inside the parent's row).
//!
//! ## Plan resolution chain
//!
//! The tab resolves which plan to show via a three-stage fallback:
//! 1. `last_executed_plan_id` — the plan most recently started/completed.
//! 2. First `Executing` plan in `conversation.plans` (defensive).
//! 3. Most recently completed plan by `max(task.completed_at_ms)`.
//!
//! The rows are plain text: every scalar a plan carries (titles, agent names) is untrusted
//! model output and is sanitized by the painter at the span builder.

use crate::adapters::tui::text_geometry as geom;
use crate::domain::models::plan::{Plan, PlanStatus, PlanSubTask, PlanTask, PlanTaskStatus};
use crate::domain::services::plan_runtime::format_elapsed_ms;

/// A plan scalar (title, agent name) as one safe line: controls, ESC/OSC sequences and bidi
/// overrides never reach a row, and a newline can never fabricate a second fact row (7D-01).
#[must_use]
pub fn safe(text: &str) -> String {
    crate::domain::services::peer_text::sanitize_display_line(text)
}

/// The glyph a task or sub-task row opens with.
#[must_use]
pub fn status_glyph(status: PlanTaskStatus) -> &'static str {
    match status {
        PlanTaskStatus::Pending => "\u{23F3}",
        PlanTaskStatus::Running => "\u{25CF}",
        PlanTaskStatus::Waiting => "\u{29D6}",
        PlanTaskStatus::Completed => "\u{2713}",
        PlanTaskStatus::Failed => "\u{2717}",
        PlanTaskStatus::Skipped => "\u{23ED}",
        PlanTaskStatus::Cancelled => "\u{2298}",
        PlanTaskStatus::Paused => "\u{23F8}",
    }
}

/// The status as a word (the window header, the detail preview).
#[must_use]
pub fn status_word(status: PlanTaskStatus) -> &'static str {
    match status {
        PlanTaskStatus::Pending => "Pending",
        PlanTaskStatus::Running => "Running",
        PlanTaskStatus::Waiting => "Waiting",
        PlanTaskStatus::Completed => "Completed",
        PlanTaskStatus::Failed => "Failed",
        PlanTaskStatus::Skipped => "Skipped",
        PlanTaskStatus::Cancelled => "Cancelled",
        PlanTaskStatus::Paused => "Paused",
    }
}

/// The plan's status as a word.
#[must_use]
pub fn plan_status_word(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Executing => "executing",
        PlanStatus::Completed => "complete",
        PlanStatus::Cancelled => "cancelled",
        _ => "not started",
    }
}

fn suffix(status: PlanTaskStatus, elapsed_ms: i64, waiting_on: &[u32]) -> String {
    match status {
        PlanTaskStatus::Pending => String::new(),
        PlanTaskStatus::Running => format!(" (running {})", format_elapsed_ms(elapsed_ms)),
        PlanTaskStatus::Waiting if waiting_on.is_empty() => " (waiting)".to_owned(),
        PlanTaskStatus::Waiting => {
            let deps = waiting_on
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!(" (deps: {deps})")
        }
        PlanTaskStatus::Completed | PlanTaskStatus::Failed => {
            format!(" ({})", format_elapsed_ms(elapsed_ms))
        }
        PlanTaskStatus::Skipped => " (skipped)".to_owned(),
        PlanTaskStatus::Cancelled => " (cancelled)".to_owned(),
        PlanTaskStatus::Paused => " (paused)".to_owned(),
    }
}

/// `[delegated → <agent>]`, the name inside a 12-cell budget.
fn delegation(info: &crate::domain::models::plan::DelegationInfo) -> String {
    format!(
        " [delegated \u{2192} {}]",
        geom::ellipsize(&safe(&info.agent_name), 12, "\u{2026}")
    )
}

/// One task as one row: number, status glyph, title, status suffix, delegation, dependencies
/// and the sub-task fraction.
#[must_use]
pub fn row_text(task: &PlanTask) -> String {
    let elapsed = task.elapsed_ms().unwrap_or(0);
    let mut text = format!(
        "{}. {} {}{}",
        task.number,
        status_glyph(task.status),
        safe(&task.title),
        suffix(task.status, elapsed, &task.waiting_on)
    );
    if let Some(info) = &task.delegated_to {
        text.push_str(&delegation(info));
    }
    if !task.depends_on.is_empty() {
        let deps = task
            .depends_on
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(" deps: {deps}"));
    }
    if task.has_sub_tasks() {
        let (completed, total) = task.sub_task_progress();
        text.push_str(&format!(" ({completed}/{total} sub-tasks)"));
    }
    text
}

/// One sub-task as one row, indented under its task.
#[must_use]
pub fn sub_row_text(sub: &PlanSubTask) -> String {
    let elapsed = sub.elapsed_ms().unwrap_or(0);
    let mut text = format!(
        "    {}. {} {}{}",
        sub.number,
        status_glyph(sub.status),
        safe(&sub.title),
        suffix(sub.status, elapsed, &[])
    );
    if let Some(info) = &sub.delegated_to {
        text.push_str(&delegation(info));
    }
    text
}

/// What `p` does to a task in this status (`None`: the key is not offered).
#[must_use]
pub fn pause_label(status: PlanTaskStatus) -> Option<&'static str> {
    match status {
        PlanTaskStatus::Pending | PlanTaskStatus::Running => Some("Pause"),
        PlanTaskStatus::Paused => Some("Resume"),
        _ => None,
    }
}

/// `s` skips a task that has not run yet or failed.
#[must_use]
pub fn can_skip(status: PlanTaskStatus) -> bool {
    matches!(status, PlanTaskStatus::Pending | PlanTaskStatus::Failed)
}

/// `r` and `e` act on a failed task only.
#[must_use]
pub fn can_retry(status: PlanTaskStatus) -> bool {
    status == PlanTaskStatus::Failed
}

pub fn resolve_panel_plan<'a>(
    conversation: &'a crate::domain::models::Conversation,
    last_id: Option<&str>,
) -> Option<&'a Plan> {
    last_id
        .and_then(|id| conversation.plans.get(id))
        .or_else(|| {
            conversation
                .plans
                .values()
                .filter(|p| p.status == PlanStatus::Executing)
                .max_by_key(|p| (p.created_at, p.id.as_str()))
        })
        .or_else(|| {
            conversation.plans.values().max_by_key(|p| {
                let last_completed = p
                    .tasks
                    .iter()
                    .filter_map(|t| t.completed_at_ms)
                    .max()
                    .unwrap_or(0);
                (last_completed, p.created_at, p.id.as_str())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::Conversation;
    use crate::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus};

    fn make_plan(status: PlanStatus, tasks: Vec<PlanTask>) -> Plan {
        Plan {
            id: "test-plan".to_string(),
            title: "Test Plan".to_string(),
            tasks,
            estimated_effort: None,
            status,
            created_at: 1000,
            resolved_at: None,
            host_message_id: None,
        }
    }

    fn make_task(number: u32, title: &str, status: PlanTaskStatus) -> PlanTask {
        PlanTask {
            number,
            title: title.to_string(),
            description: String::new(),
            depends_on: vec![],
            status,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        }
    }

    #[test]
    fn story21_7_a_task_row_is_one_line_with_glyph_suffix_delegation_and_deps() {
        let mut task = make_task(2, "Review code", PlanTaskStatus::Running);
        task.delegated_to = Some(crate::domain::models::plan::DelegationInfo {
            agent_name: "code-reviewer-with-a-long-name".to_string(),
            agent_id: None,
            delegated_at_ms: 1_700_000_000,
            spool_task_id: None,
        });
        task.depends_on = vec![1];
        let row = row_text(&task);
        assert!(
            row.starts_with("2. \u{25CF} Review code (running "),
            "{row}"
        );
        assert!(
            row.contains("[delegated \u{2192} code-review\u{2026}]"),
            "the agent name keeps a 12-cell budget: {row}"
        );
        assert!(row.ends_with(" deps: 1"), "{row}");
        assert!(!row.contains('\n'));
        assert_eq!(
            row_text(&make_task(1, "Setup", PlanTaskStatus::Completed)),
            "1. \u{2713} Setup (0s)"
        );
        assert_eq!(
            row_text(&make_task(3, "Build", PlanTaskStatus::Pending)),
            "3. \u{23F3} Build"
        );
    }

    #[test]
    fn story21_7_a_sub_task_is_its_own_indented_row_and_the_parent_shows_the_fraction() {
        use crate::domain::models::plan::PlanSubTask;
        let sub = |number: u32, status: PlanTaskStatus| PlanSubTask {
            number,
            title: format!("part {number}"),
            description: String::new(),
            status,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            delegated_to: None,
        };
        let mut task = make_task(1, "Build", PlanTaskStatus::Running);
        task.sub_tasks = vec![
            sub(1, PlanTaskStatus::Completed),
            sub(2, PlanTaskStatus::Pending),
        ];
        assert!(row_text(&task).ends_with(" (1/2 sub-tasks)"));
        assert_eq!(
            sub_row_text(&task.sub_tasks[1]),
            "    2. \u{23F3} part 2",
            "indented under its task, one line"
        );
    }

    #[test]
    fn story21_7_the_status_rules_of_the_keys() {
        use PlanTaskStatus::{
            Cancelled, Completed, Failed, Paused, Pending, Running, Skipped, Waiting,
        };
        for (status, pause, skip, retry) in [
            (Pending, Some("Pause"), true, false),
            (Running, Some("Pause"), false, false),
            (Paused, Some("Resume"), false, false),
            (Failed, None, true, true),
            (Completed, None, false, false),
            (Skipped, None, false, false),
            (Cancelled, None, false, false),
            (Waiting, None, false, false),
        ] {
            assert_eq!(pause_label(status), pause, "{status:?}");
            assert_eq!(can_skip(status), skip, "{status:?}");
            assert_eq!(can_retry(status), retry, "{status:?}");
        }
    }

    #[test]
    fn resolve_panel_plan_prefers_last_id() {
        let mut conv = Conversation {
            id: "conv1".to_string(),
            title: String::new(),
            messages: vec![],
            turns: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
        };
        let p1 = make_plan(
            PlanStatus::Completed,
            vec![make_task(1, "A", PlanTaskStatus::Completed)],
        );
        let p2 = make_plan(
            PlanStatus::Executing,
            vec![make_task(1, "B", PlanTaskStatus::Running)],
        );
        conv.plans.insert("p1".to_string(), p1);
        conv.plans.insert("p2".to_string(), p2);
        let result = resolve_panel_plan(&conv, Some("p1"));
        assert!(result.is_some());
        assert_eq!(result.unwrap().tasks[0].title, "A");
    }

    #[test]
    fn resolve_panel_plan_fallback_executing() {
        let mut conv = Conversation {
            id: "conv1".to_string(),
            title: String::new(),
            messages: vec![],
            turns: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
        };
        let p2 = make_plan(
            PlanStatus::Executing,
            vec![make_task(1, "B", PlanTaskStatus::Running)],
        );
        conv.plans.insert("p2".to_string(), p2);
        let result = resolve_panel_plan(&conv, Some("nonexistent"));
        assert!(result.is_some());
        assert_eq!(result.unwrap().tasks[0].title, "B");
    }
}
