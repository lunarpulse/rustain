//! The mission strip (Story 21.7 A22, AC27): one muted row directly above the status row that
//! says what the session is doing at a glance — `Agents <running>/<total> · Tasks <done>/<total>
//! · Decisions <n>` — and, when nothing applies, the generated key hint that opens Mission
//! Control. It has no focus and no keys; it is painted by the shared compositor in both hosts.
//!
//! Every number is a count of something the session already holds, read once per draw in
//! O(agents + tasks) with no allocation beyond the row itself; no peer, model or tool text is ever
//! put on the row. The decision count is the status chip's own value (`decision_queue::waiting`,
//! and the read-only mirror's `mirror_waiting`), never a second count. The row never contains the
//! words the native drivers wait on (`Ready`, `Needs you`, `Read-only.`, `Decision:`, `waiting`).

use ratatui::prelude::*;

use crate::adapters::tui::decision_queue;
use crate::adapters::tui::keymap::Host;
use crate::adapters::tui::keymap::render::leader_follow_up_compact;
use crate::adapters::tui::keymap::table::LeaderCmd;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::task_panel;
use crate::domain::models::{Conversation, NodeState, PlanTaskStatus};

/// Between two segments.
const SEPARATOR: &str = " \u{B7} ";

/// The decisions a session is waiting on, from the chip's producers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionCount {
    /// This session's own queue (`Decisions <n>`).
    Waiting(usize),
    /// What the writer of a read-only mirror is waiting on (`Writer decisions <n>`).
    Mirror(usize),
}

/// What the strip says; a `None` segment does not apply and is not painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MissionFacts {
    /// `(running, total)` of the subagent roster (local, roster non-empty).
    pub agents: Option<(usize, usize)>,
    /// `(done, total)` of the resolved plan of the active conversation (local).
    pub tasks: Option<(usize, usize)>,
    /// Decisions waiting (omitted at zero).
    pub decisions: Option<DecisionCount>,
}

/// Read the facts of one draw. The attached host projects no agents or tasks (A20), so it
/// reports only decisions; `stage` is the active conversation of the local host.
#[must_use]
pub fn facts(state: &TuiState, host: Host, stage: Option<&Conversation>) -> MissionFacts {
    let waiting = decision_queue::waiting(state);
    let mirror = state.decisions.mirror_waiting();
    let decisions = if waiting > 0 {
        Some(DecisionCount::Waiting(waiting))
    } else if mirror > 0 {
        Some(DecisionCount::Mirror(mirror))
    } else {
        None
    };
    if host == Host::Attached {
        return MissionFacts {
            decisions,
            ..MissionFacts::default()
        };
    }
    let roster = &state.agent_panel_state.cached_entries;
    let agents = (!roster.is_empty()).then(|| {
        let running = roster
            .iter()
            .filter(|entry| entry.current_status == NodeState::Running)
            .count();
        (running, roster.len())
    });
    let tasks = stage
        .and_then(|conversation| {
            task_panel::resolve_panel_plan(
                conversation,
                state.task_panel_state.last_executed_plan_id.as_deref(),
            )
        })
        .filter(|plan| !plan.tasks.is_empty())
        .map(|plan| {
            let done = plan
                .tasks
                .iter()
                .filter(|task| task.status == PlanTaskStatus::Completed)
                .count();
            (done, plan.tasks.len())
        });
    MissionFacts {
        agents,
        tasks,
        decisions,
    }
}

/// Append one segment (after the separator when it is not the first) and note where it ends.
fn segment(out: &mut String, ends: &mut [usize; 3], count: &mut usize, text: std::fmt::Arguments) {
    use std::fmt::Write as _;
    if *count > 0 {
        out.push_str(SEPARATOR);
    }
    let _ = out.write_fmt(text);
    ends[*count] = out.len();
    *count += 1;
}

/// Write the applicable segments into `out` (cleared first), full labels or abbreviated; the
/// byte offset at which each written segment ends goes to `ends`. Returns how many were written.
fn build(facts: &MissionFacts, short: bool, out: &mut String, ends: &mut [usize; 3]) -> usize {
    out.clear();
    let mut count = 0;
    if let Some((running, total)) = facts.agents {
        let label = if short { "A" } else { "Agents" };
        segment(
            out,
            ends,
            &mut count,
            format_args!("{label} {running}/{total}"),
        );
    }
    if let Some((done, total)) = facts.tasks {
        let label = if short { "T" } else { "Tasks" };
        segment(
            out,
            ends,
            &mut count,
            format_args!("{label} {done}/{total}"),
        );
    }
    match facts.decisions {
        Some(DecisionCount::Waiting(n)) => {
            let label = if short { "D" } else { "Decisions" };
            segment(out, ends, &mut count, format_args!("{label} {n}"));
        }
        Some(DecisionCount::Mirror(n)) => {
            let label = if short { "W" } else { "Writer decisions" };
            segment(out, ends, &mut count, format_args!("{label} {n}"));
        }
        None => {}
    }
    count
}

/// The key hint shown when no segment applies, generated from the registry: the leader row that
/// opens Mission Control (local, `Ctrl+X s`) or its only tab in the attached host (`Ctrl+X l`).
fn idle_hint(host: Host) -> String {
    match host {
        Host::Local => format!(
            "{} Mission Control",
            leader_follow_up_compact(LeaderCmd::Agents)
        ),
        Host::Attached => format!("{} Messages", leader_follow_up_compact(LeaderCmd::A2aLog)),
    }
}

/// The row text for a strip `width` cells wide: the full labels when they fit, else the
/// abbreviations, then segments dropped right to left; the hint is dropped when it does not fit.
#[must_use]
pub fn text(facts: &MissionFacts, host: Host, width: u16) -> String {
    let width = usize::from(width);
    let mut out = String::with_capacity(48);
    let mut ends = [0usize; 3];
    let mut count = build(facts, false, &mut out, &mut ends);
    if count == 0 {
        let hint = idle_hint(host);
        return if text_geometry::cells(&hint) <= width {
            hint
        } else {
            String::new()
        };
    }
    if text_geometry::cells(&out) > width {
        count = build(facts, true, &mut out, &mut ends);
        while count > 1 && text_geometry::cells(&out[..ends[count - 1]]) > width {
            count -= 1;
        }
        out.truncate(ends[count - 1]);
        if text_geometry::cells(&out) > width {
            let kept = text_geometry::clip(&out, width).len();
            out.truncate(kept);
        }
    }
    out
}

/// The strip row: muted, left-aligned, one line, never wrapped.
#[must_use]
pub fn line(facts: &MissionFacts, host: Host, width: u16, theme: &Theme) -> Line<'static> {
    Line::styled(
        text(facts, host, width),
        Style::default().fg(theme.colors.fg_muted),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> MissionFacts {
        MissionFacts {
            agents: Some((1, 3)),
            tasks: Some((4, 7)),
            decisions: Some(DecisionCount::Waiting(2)),
        }
    }

    #[test]
    fn story21_7_the_grammar_is_agents_tasks_decisions_and_shortens_then_drops_right_to_left() {
        let facts = all();
        assert_eq!(
            text(&facts, Host::Local, 60),
            "Agents 1/3 \u{B7} Tasks 4/7 \u{B7} Decisions 2"
        );
        // 29 cells: the abbreviations fit where the labels do not.
        assert_eq!(
            text(&facts, Host::Local, 29),
            "A 1/3 \u{B7} T 4/7 \u{B7} D 2"
        );
        assert_eq!(text(&facts, Host::Local, 18), "A 1/3 \u{B7} T 4/7");
        assert_eq!(text(&facts, Host::Local, 5), "A 1/3");
        assert_eq!(text(&facts, Host::Local, 3), "A 1");
    }

    #[test]
    fn story21_7_only_meaningful_segments_are_painted_and_a_mirror_names_the_writer() {
        let mirror = MissionFacts {
            decisions: Some(DecisionCount::Mirror(2)),
            ..MissionFacts::default()
        };
        assert_eq!(text(&mirror, Host::Attached, 80), "Writer decisions 2");
        assert_eq!(text(&mirror, Host::Attached, 12), "W 2");
        let tasks = MissionFacts {
            tasks: Some((1, 3)),
            ..MissionFacts::default()
        };
        assert_eq!(text(&tasks, Host::Local, 80), "Tasks 1/3");
    }

    #[test]
    fn story21_7_an_idle_strip_shows_the_generated_hint_of_each_host() {
        let idle = MissionFacts::default();
        assert_eq!(text(&idle, Host::Local, 60), "^X s Mission Control");
        assert_eq!(text(&idle, Host::Attached, 60), "^X l Messages");
        assert_eq!(
            text(&idle, Host::Local, 10),
            "",
            "a hint that does not fit is dropped"
        );
    }

    #[test]
    fn story21_7_no_row_contains_a_driver_wait_string() {
        for facts in [all(), MissionFacts::default()] {
            for host in [Host::Local, Host::Attached] {
                for width in [60u16, 80, 130] {
                    let row = text(&facts, host, width);
                    for word in ["Ready", "Needs you", "Read-only.", "Decision:", "waiting"] {
                        assert!(!row.contains(word), "{word:?} in {row:?}");
                    }
                }
            }
        }
    }
}
