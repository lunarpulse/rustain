//! The task window's header (Story 21.7 A8; Story 6.3's chat-pane drill-down became a window).
//!
//! `facts` is the old drill-down's header rows as plain text, one fact per line: the task line
//! (number, title, status, elapsed), the plan, the dependencies, the delegation, the sub-task
//! fraction and the result's token and tool-call counts. The window lays them out through the
//! shared text window (sanitized line by line at the builder, wrapped at the frame's inner
//! width); the body under them is the result (`markdown_body::layout`), or the error and skip
//! reason literal. The action row of the old view is the window's generated rail.
//!
//! The drill-down painted into the chat pane in place of the Stage; nothing paints a task into
//! the chat pane any more.

use super::task_panel::{safe, status_glyph, status_word};
use crate::domain::models::plan::{Plan, PlanTask};
use crate::domain::services::plan_runtime::format_elapsed_ms;

/// The facts of one task, one per line.
#[must_use]
pub fn facts(plan: &Plan, task: &PlanTask) -> String {
    let elapsed = format_elapsed_ms(task.elapsed_ms().unwrap_or(0));
    let mut out = format!(
        "Task {}. {}    {} {} \u{B7} {}",
        task.number,
        safe(&task.title),
        status_glyph(task.status),
        status_word(task.status),
        elapsed
    );
    out.push_str(&format!("\nPlan: {}", safe(&plan.title)));
    if !task.depends_on.is_empty() {
        let deps: Vec<String> = task.depends_on.iter().map(u32::to_string).collect();
        out.push_str(&format!("\nDepends on: {}", deps.join(", ")));
    }
    if let Some(info) = &task.delegated_to {
        out.push_str(&format!("\nDelegated to: {}", safe(&info.agent_name)));
    }
    if task.has_sub_tasks() {
        let (done, total) = task.sub_task_progress();
        out.push_str(&format!("\nSub-tasks: {done}/{total} done"));
        for sub in &task.sub_tasks {
            out.push_str(&format!(
                "\n  {}. {} {}",
                sub.number,
                status_glyph(sub.status),
                safe(&sub.title)
            ));
        }
    }
    if let Some(result) = &task.result {
        let tokens = result
            .token_count
            .map_or_else(|| "-".to_owned(), |n| n.to_string());
        out.push_str(&format!(
            "\nTokens: {tokens}   Tool calls: {}",
            result.tool_call_count
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::plan::{PlanStatus, PlanTaskStatus, TaskResult};

    fn plan() -> Plan {
        Plan {
            id: "p1".to_string(),
            title: "Test Plan".to_string(),
            tasks: vec![],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 1000,
            resolved_at: None,
            host_message_id: None,
            origin_restrictions: None,
        }
    }

    fn task(number: u32, title: &str, status: PlanTaskStatus) -> PlanTask {
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
    fn story21_7_the_header_carries_the_old_drill_down_facts_one_per_line() {
        let mut t = task(3, "Ship it", PlanTaskStatus::Completed);
        t.depends_on = vec![1, 2];
        t.delegated_to = Some(crate::domain::models::plan::DelegationInfo {
            agent_name: "reviewer".to_string(),
            agent_id: None,
            delegated_at_ms: 1,
            spool_task_id: None,
        });
        t.result = Some(TaskResult {
            text: "done".to_string(),
            tool_call_count: 4,
            token_count: Some(120),
        });
        let facts = facts(&plan(), &t);
        let lines: Vec<&str> = facts.lines().collect();
        assert_eq!(
            lines,
            vec![
                "Task 3. Ship it    \u{2713} Completed \u{B7} 0s",
                "Plan: Test Plan",
                "Depends on: 1, 2",
                "Delegated to: reviewer",
                "Tokens: 120   Tool calls: 4",
            ]
        );
        let bare = super::facts(&plan(), &task(1, "x", PlanTaskStatus::Pending));
        assert_eq!(bare.lines().count(), 2, "no deps, delegation or result");
    }

    #[test]
    fn story21_7_7d01_untrusted_scalars_never_fabricate_a_fact_row_or_carry_controls() {
        let evil = "X\nTokens: 99999\r\u{1b}[31mred\u{1b}]0;t\u{7}\u{202e}rtl";
        let mut p = plan();
        p.title = evil.to_owned();
        let mut t = task(1, evil, PlanTaskStatus::Pending);
        t.delegated_to = Some(crate::domain::models::plan::DelegationInfo {
            agent_name: evil.to_owned(),
            agent_id: None,
            delegated_at_ms: 1,
            spool_task_id: None,
        });
        t.sub_tasks = vec![crate::domain::models::plan::PlanSubTask {
            number: 1,
            title: evil.to_owned(),
            description: String::new(),
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            delegated_to: None,
        }];
        let facts = super::facts(&p, &t);
        assert!(
            !facts.lines().any(|line| line.starts_with("Tokens: 99999")),
            "a newline in a title fabricated a fact row: {facts:?}"
        );
        for bad in ['\u{1b}', '\u{7}', '\r', '\u{202e}'] {
            assert!(!facts.contains(bad), "{bad:?} reached the facts: {facts:?}");
        }
        // Header, Plan, Delegated to, Sub-tasks, one sub-task row: exactly five lines.
        assert_eq!(facts.lines().count(), 5, "{facts:?}");
        let row = super::super::task_panel::row_text(&t);
        assert!(!row.contains('\n') && !row.contains('\u{1b}'), "{row:?}");
    }
}
