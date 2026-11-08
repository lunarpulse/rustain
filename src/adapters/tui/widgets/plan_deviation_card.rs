//! Plan deviation body — Story 6.4: the reapproval rows of the `Decision: Plan
//! deviation` frame (Story 21.6 6-C). Keys: the frame's rail (`[y] Approve  [e] Edit
//! [n] Reject`).

use ratatui::prelude::*;

/// The deviation rows: what changed and the runtime's summary. `current_step_count` is the
/// task count after the deviation.
pub fn lines(
    original_step_count: u32,
    current_step_count: u32,
    changed_steps: &[u32],
    summary: &str,
) -> Vec<Line<'static>> {
    let skipped: Vec<String> = changed_steps.iter().map(|n| n.to_string()).collect();
    vec![
        Line::from("Plan deviation — reapproval required"),
        Line::from(format!(
            "{} tasks → {} tasks. Skipped: {}",
            original_step_count,
            current_step_count,
            skipped.join(", ")
        )),
        Line::from(summary.to_owned()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_6_the_rows_name_the_counts_the_skipped_tasks_and_the_summary() {
        let rows: Vec<String> = lines(
            5,
            5,
            &[3, 4],
            "2 task(s) auto-skipped due to upstream failure.",
        )
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
        assert_eq!(
            rows,
            [
                "Plan deviation — reapproval required",
                "5 tasks → 5 tasks. Skipped: 3, 4",
                "2 task(s) auto-skipped due to upstream failure."
            ]
        );
    }
}
