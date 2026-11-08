//! Cancel plan confirmation body — Story 6.4: the rows of the `Decision: Cancel plan`
//! frame (Story 21.6 6-C). Keys: the frame's rail (`[y] Confirm  [n] Decline`).

use ratatui::prelude::*;

/// The plan's title and what cancelling leaves behind.
pub fn lines(plan_title: &str, n_pending: u32, n_completed: u32) -> Vec<Line<'static>> {
    vec![
        Line::from(plan_title.to_owned()),
        Line::from(format!(
            "{} task(s) remaining, {} completed (will be kept)",
            n_pending, n_completed
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_6_the_rows_name_the_plan_and_the_counts() {
        let rows: Vec<String> = lines("Ship it", 3, 2)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            rows,
            ["Ship it", "3 task(s) remaining, 2 completed (will be kept)"]
        );
    }
}
