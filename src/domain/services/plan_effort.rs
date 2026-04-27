//! Effort estimate derivation for plan proposals (Story 6-1a AC3).
//!
//! When the model omits `estimated_tool_calls` and `estimated_seconds`, this
//! heuristic fills conservative defaults: one tool call per task, ~8s per task.

use crate::domain::models::{EffortEstimate, PlanTask};

/// Derives a conservative effort estimate from the task list.
/// Returns `None` for an empty task slice (defensive — callers should validate
/// non-empty tasks before invoking).
pub fn derive_effort_estimate(tasks: &[PlanTask]) -> Option<EffortEstimate> {
    if tasks.is_empty() {
        return None;
    }
    let n = tasks.len() as u32;
    Some(EffortEstimate {
        tool_calls: Some(n),
        seconds: n.checked_mul(8),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::PlanTaskStatus;

    fn make_task(number: u32, title: &str) -> PlanTask {
        PlanTask {
            number,
            title: title.to_string(),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
        }
    }

    #[test]
    fn empty_tasks_returns_none() {
        assert_eq!(derive_effort_estimate(&[]), None);
    }

    #[test]
    fn five_tasks_returns_estimate() {
        let tasks: Vec<PlanTask> = (1..=5).map(|i| make_task(i, "t")).collect();
        let est = derive_effort_estimate(&tasks).unwrap();
        assert_eq!(est.tool_calls, Some(5));
        assert_eq!(est.seconds, Some(40));
    }

    #[test]
    fn single_task_returns_estimate() {
        let tasks = vec![make_task(1, "t")];
        let est = derive_effort_estimate(&tasks).unwrap();
        assert_eq!(est.tool_calls, Some(1));
        assert_eq!(est.seconds, Some(8));
    }
}
