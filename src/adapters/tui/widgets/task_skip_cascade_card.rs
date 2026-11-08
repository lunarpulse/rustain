//! Task skip cascade body — Story 6.4: the row of the `Decision: Skip cascade` frame
//! (Story 21.6 6-C). Keys: the frame's rail (`[s] Skip cascade  [a] Continue anyway
//! [n] Cancel skip`).

use ratatui::prelude::*;

use crate::adapters::tui::decision_queue::SkipCascadePending;

/// What skipping the task leaves depending on it.
pub fn lines(pending: &SkipCascadePending) -> Vec<Line<'static>> {
    vec![Line::from(format!(
        "Task {} skipped — {} task(s) depend on it.",
        pending.source_task,
        pending.downstream.len()
    ))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::plan::PlanTaskStatus;

    #[test]
    fn story21_6_the_row_names_the_task_and_its_dependants() {
        let pending = SkipCascadePending {
            plan_id: "p".into(),
            source_task: 2,
            source_prior_status: PlanTaskStatus::Pending,
            source_prior_error: None,
            downstream: vec![3, 4, 5],
        };
        let rows: Vec<String> = lines(&pending)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(rows, ["Task 2 skipped — 3 task(s) depend on it."]);
    }
}
