//! The Tasks tab's rows (Story 21.7 A6, A11, AC16): the resolved plan of the **active
//! conversation**, one list row per task and one per sub-task, keyed by id.
//!
//! The plan lives in the conversation, which only the host's event loop holds, so the host
//! calls [`sync`] before it paints a hub that shows Tasks (and after a Tasks action): the rows
//! are rebuilt from the conversation it passes, and only a row set that actually changed bumps
//! the list's revision (an unchanged repaint rebuilds nothing the caches key on).
//!
//! Every action carries a [`TaskRef`] — `(conversation, plan, task number)` — taken from the
//! rows as they were last built, never from "whatever is active when the key is handled": a
//! tab switch between the paint and the key is refused by the host (PC-3), not misapplied to
//! the plan of another conversation. A sub-task row resolves to its parent task.

use super::{McRow, McTab};
use crate::adapters::tui::state::{Reorder, TuiState};
use crate::adapters::tui::widgets::{task_detail, task_panel};
use crate::domain::models::Conversation;
use crate::domain::models::plan::{Plan, PlanStatus, PlanSubTask, PlanTask, PlanTaskStatus};

/// Which task of which plan of which conversation an action is for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskRef {
    pub conversation_id: String,
    pub plan_id: String,
    pub number: u32,
}

/// The zero state (the old panel's words).
pub const EMPTY: &str = "No active plan. Start one by asking for a complex task.";
/// A plan with nothing to list.
const NO_TASKS: &str = "This plan has no tasks.";
/// Rows of one plan that are listed (a bound on the work of one rebuild and on the list).
pub const MAX_ROWS: usize = 500;
/// Bytes of a result, error or description a row's detail preview carries.
const EXCERPT_BYTES: usize = 2_048;

/// What the tab was last built from: the conversation (its title for the refusal text), the
/// plan, and the status of every task row (what the rail may offer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TasksSource {
    pub conversation_id: String,
    pub conversation_title: String,
    pub plan_id: Option<String>,
    pub plan_executing: bool,
    /// `(row id, task number, status)` of the task rows.
    tasks: Vec<(String, u32, PlanTaskStatus)>,
}

impl TasksSource {
    /// The status of the task a row id names (a sub-task row names its parent task).
    #[must_use]
    pub fn status_of(&self, id: &str) -> Option<PlanTaskStatus> {
        let (plan_id, number) = parse_id(id)?;
        if self.plan_id.as_deref() != Some(plan_id.as_str()) {
            return None;
        }
        self.tasks
            .iter()
            .find(|(_, n, _)| *n == number)
            .map(|(_, _, status)| *status)
    }
}

/// What the tab shows.
pub struct TasksList {
    pub rows: Vec<McRow>,
    pub empty: Option<String>,
    pub note: Option<String>,
    pub source: TasksSource,
}

/// The id of a task row (`<plan>#<number>`) and of a sub-task row (`<plan>#<number>.<sub>`).
#[must_use]
pub fn task_id(plan_id: &str, number: u32) -> String {
    format!("{plan_id}#{number}")
}

fn sub_id(plan_id: &str, number: u32, sub: u32) -> String {
    format!("{plan_id}#{number}.{sub}")
}

/// The plan and (parent) task number a row id names.
#[must_use]
pub fn parse_id(id: &str) -> Option<(String, u32)> {
    let (plan_id, rest) = id.rsplit_once('#')?;
    let number = rest.split('.').next()?.parse().ok()?;
    Some((plan_id.to_owned(), number))
}

/// At most `max` bytes of `text`, cut at a character boundary, with a marker when cut.
fn excerpt(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", &text[..end])
}

fn task_detail_text(plan: &Plan, task: &PlanTask) -> String {
    let mut out = task_detail::facts(plan, task);
    let body = match task.status {
        PlanTaskStatus::Completed => task.result.as_ref().map(|result| result.text.as_str()),
        PlanTaskStatus::Failed | PlanTaskStatus::Skipped | PlanTaskStatus::Cancelled => {
            task.error.as_deref()
        }
        _ => None,
    };
    if let Some(body) = body.filter(|body| !body.is_empty()) {
        out.push_str("\n\n");
        out.push_str(&excerpt(body, EXCERPT_BYTES));
    } else if !task.description.is_empty() {
        out.push_str("\n\n");
        out.push_str(&excerpt(&task.description, EXCERPT_BYTES));
    }
    out
}

fn sub_detail_text(task: &PlanTask, sub: &PlanSubTask) -> String {
    let mut out = format!(
        "Sub-task {} of task {}. {}\n{}",
        sub.number,
        task.number,
        task_panel::safe(&task.title),
        task_panel::sub_row_text(sub).trim_start()
    );
    let body = match sub.status {
        PlanTaskStatus::Completed => sub.result.as_ref().map(|result| result.text.as_str()),
        PlanTaskStatus::Failed | PlanTaskStatus::Skipped | PlanTaskStatus::Cancelled => {
            sub.error.as_deref()
        }
        _ => None,
    };
    if let Some(body) = body.filter(|body| !body.is_empty()) {
        out.push_str("\n\n");
        out.push_str(&excerpt(body, EXCERPT_BYTES));
    }
    out
}

/// Build the tab from the active conversation: its resolved plan (the plan the last task event
/// named, else the executing one, else the latest), the task being reordered marked.
#[must_use]
pub fn list(
    conversation: &Conversation,
    last_plan_id: Option<&str>,
    reorder: Option<&Reorder>,
) -> TasksList {
    let mut source = TasksSource {
        conversation_id: conversation.id.clone(),
        conversation_title: conversation.title.clone(),
        ..TasksSource::default()
    };
    let Some(plan) = task_panel::resolve_panel_plan(conversation, last_plan_id) else {
        return TasksList {
            rows: Vec::new(),
            empty: Some(EMPTY.to_owned()),
            note: None,
            source,
        };
    };
    source.plan_id = Some(plan.id.clone());
    source.plan_executing = plan.status == PlanStatus::Executing;
    let reordering = reorder
        .filter(|reorder| reorder.conversation_id == conversation.id && reorder.plan_id == plan.id)
        .map(|reorder| reorder.number);
    let mut rows = Vec::new();
    let mut capped = false;
    'tasks: for task in &plan.tasks {
        if rows.len() >= MAX_ROWS {
            capped = true;
            break;
        }
        let mut text = task_panel::row_text(task);
        if reordering == Some(task.number) {
            text = format!("\u{2195} {text}");
        }
        let id = task_id(&plan.id, task.number);
        source.tasks.push((id.clone(), task.number, task.status));
        rows.push(McRow::new(&id, &text, &task_detail_text(plan, task)));
        for sub in &task.sub_tasks {
            if rows.len() >= MAX_ROWS {
                capped = true;
                break 'tasks;
            }
            rows.push(McRow::new(
                &sub_id(&plan.id, task.number, sub.number),
                &task_panel::sub_row_text(sub),
                &sub_detail_text(task, sub),
            ));
        }
    }
    let status = task_panel::plan_status_word(plan.status);
    let mut note = format!("{} \u{B7} {status}", task_panel::safe(&plan.title));
    if matches!(plan.status, PlanStatus::Completed | PlanStatus::Cancelled) {
        note.push_str(" \u{2014} open a new plan to refresh");
    }
    if capped {
        note.push_str(&format!(" \u{2014} the first {MAX_ROWS} rows are listed"));
    }
    TasksList {
        empty: rows.is_empty().then(|| NO_TASKS.to_owned()),
        rows,
        note: Some(note),
        source,
    }
}

/// `Enter`, `p`, `s`, `x` on the selected row: the reference the host resolves.
#[must_use]
pub fn selected_ref(state: &TuiState) -> Option<TaskRef> {
    let mission = &state.mission_control;
    if mission.tab != McTab::Tasks {
        return None;
    }
    let source = mission.tasks.as_ref()?;
    let (plan_id, number) = parse_id(mission.selected_id()?)?;
    (source.plan_id.as_deref() == Some(plan_id.as_str())).then(|| TaskRef {
        conversation_id: source.conversation_id.clone(),
        plan_id,
        number,
    })
}

/// The status of the selected row's task, as the rows were last built.
#[must_use]
pub fn selected_status(state: &TuiState) -> Option<PlanTaskStatus> {
    let mission = &state.mission_control;
    if mission.tab != McTab::Tasks {
        return None;
    }
    mission.tasks.as_ref()?.status_of(mission.selected_id()?)
}

/// A result or error as `(length, first and last 64 bytes)`: cheap, allocation-free and
/// changes whenever an edit, retry or new result changes either end or the length.
fn sample(text: &str) -> (usize, &[u8], &[u8]) {
    let bytes = text.as_bytes();
    (
        bytes.len(),
        &bytes[..bytes.len().min(64)],
        &bytes[bytes.len().saturating_sub(64)..],
    )
}

/// Hash what a task's rows, detail preview and window facts depend on, without copying any
/// text: scalars, a length-and-ends sample of the result/error, and the elapsed seconds of a
/// running task or sub-task.
pub fn hash_task(task: &PlanTask, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    (
        task.number,
        task.status as u8,
        task.started_at_ms,
        task.completed_at_ms,
        task.title.as_str(),
        &task.depends_on,
        &task.waiting_on,
        task.description.len(),
    )
        .hash(hasher);
    if task.status == PlanTaskStatus::Running {
        (task.elapsed_ms().unwrap_or(0) / 1_000).hash(hasher);
    }
    if let Some(result) = &task.result {
        (
            sample(&result.text),
            result.tool_call_count,
            result.token_count,
        )
            .hash(hasher);
    }
    if let Some(error) = &task.error {
        sample(error).hash(hasher);
    }
    if let Some(info) = &task.delegated_to {
        info.agent_name.hash(hasher);
    }
    for sub in &task.sub_tasks {
        (
            sub.number,
            sub.status as u8,
            sub.title.as_str(),
            sub.result.as_ref().map(|r| sample(&r.text)),
            sub.error.as_deref().map(sample),
        )
            .hash(hasher);
        if sub.status == PlanTaskStatus::Running {
            (sub.elapsed_ms().unwrap_or(0) / 1_000).hash(hasher);
        }
    }
}

/// What the Tasks rows depend on (conversation, resolved plan, every task, the reorder mark).
fn fingerprint(
    conversation: &Conversation,
    last_plan_id: Option<&str>,
    reorder: Option<&Reorder>,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    conversation.id.hash(&mut hasher);
    conversation.title.hash(&mut hasher);
    match task_panel::resolve_panel_plan(conversation, last_plan_id) {
        Some(plan) => {
            (
                plan.id.as_str(),
                plan.title.as_str(),
                plan.status as u8,
                plan.tasks.len(),
            )
                .hash(&mut hasher);
            for task in &plan.tasks {
                hash_task(task, &mut hasher);
            }
        }
        None => 0u8.hash(&mut hasher),
    }
    reorder
        .map(|r| (r.number, r.plan_id.as_str()))
        .hash(&mut hasher);
    hasher.finish()
}

/// The host is about to paint (or a Tasks action ran): rebuild the rows from the active
/// conversation and let an open task window follow its task. A different conversation than the
/// rows were built from starts the selection over (it names tasks of another plan). The rows
/// are rebuilt only when something they depend on changed ([`fingerprint`]): an unchanged
/// repaint allocates and copies nothing (7D-02).
pub fn sync(state: &mut TuiState, conversation: &Conversation) {
    let last = state.task_panel_state.last_executed_plan_id.as_deref();
    let fp = fingerprint(conversation, last, state.task_panel_state.reorder.as_ref());
    if state.mission_control.tasks_fp != Some(fp) {
        let list = list(conversation, last, state.task_panel_state.reorder.as_ref());
        let mission = &mut state.mission_control;
        if mission
            .tasks
            .as_ref()
            .is_some_and(|source| source.conversation_id != list.source.conversation_id)
        {
            mission.forget_selection(McTab::Tasks);
        }
        mission.set_list_if_changed(McTab::Tasks, list.rows, list.empty, list.note);
        mission.tasks = Some(list.source);
        mission.tasks_fp = Some(fp);
        mission.tasks_builds += 1;
    }
    crate::adapters::tui::task_window::refresh(state, conversation);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::plan::{PlanSubTask, TaskResult};

    fn task(number: u32, title: &str, status: PlanTaskStatus) -> PlanTask {
        PlanTask {
            number,
            title: title.to_owned(),
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

    fn conversation(plan: Plan) -> Conversation {
        Conversation {
            id: "conv-1".to_owned(),
            title: "Build".to_owned(),
            plans: std::collections::HashMap::from([(plan.id.clone(), plan)]),
            ..Conversation::default()
        }
    }

    fn plan(tasks: Vec<PlanTask>) -> Plan {
        Plan {
            id: "p1".to_owned(),
            title: "Ship".to_owned(),
            tasks,
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 1,
            resolved_at: None,
            host_message_id: None,
            origin_restrictions: None,
        }
    }

    #[test]
    fn story21_7_ids_round_trip_and_a_sub_task_names_its_parent() {
        assert_eq!(task_id("p#1", 3), "p#1#3");
        assert_eq!(parse_id("p#1#3"), Some(("p#1".to_owned(), 3)));
        assert_eq!(parse_id("p1#3.2"), Some(("p1".to_owned(), 3)));
        assert_eq!(parse_id("no-hash"), None);
        assert_eq!(parse_id("p1#x"), None);
    }

    #[test]
    fn story21_7_a_row_per_task_and_per_sub_task_beneath_its_parent_with_a_note() {
        let mut parent = task(1, "Build", PlanTaskStatus::Running);
        parent.sub_tasks = vec![PlanSubTask {
            number: 1,
            title: "compile".to_owned(),
            description: String::new(),
            status: PlanTaskStatus::Completed,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            delegated_to: None,
        }];
        let mut done = task(2, "Test", PlanTaskStatus::Completed);
        done.result = Some(TaskResult {
            text: "all green".to_owned(),
            tool_call_count: 1,
            token_count: None,
        });
        let conv = conversation(plan(vec![parent, done]));
        let built = list(&conv, Some("p1"), None);
        let ids: Vec<&str> = built.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["p1#1", "p1#1.1", "p1#2"]);
        assert!(built.rows[1].text.starts_with("    1. "), "indented");
        assert!(built.rows[2].detail.ends_with("all green"));
        assert_eq!(built.note.as_deref(), Some("Ship \u{B7} executing"));
        assert!(built.empty.is_none());
        assert_eq!(
            built.source.status_of("p1#1.1"),
            Some(PlanTaskStatus::Running)
        );
        assert_eq!(
            built.source.status_of("p1#2"),
            Some(PlanTaskStatus::Completed)
        );
        assert_eq!(built.source.status_of("other#2"), None);
    }

    #[test]
    fn story21_7_no_plan_and_no_tasks_say_so_and_the_reordered_task_is_marked() {
        let none = list(&Conversation::default(), None, None);
        assert!(none.rows.is_empty());
        assert_eq!(none.empty.as_deref(), Some(EMPTY));
        let mut empty_plan = conversation(plan(vec![]));
        empty_plan.plans.get_mut("p1").unwrap().status = PlanStatus::Completed;
        let built = list(&empty_plan, Some("p1"), None);
        assert_eq!(built.empty.as_deref(), Some(NO_TASKS));
        assert!(built.note.unwrap().ends_with("open a new plan to refresh"));
        let conv = conversation(plan(vec![
            task(1, "A", PlanTaskStatus::Pending),
            task(2, "B", PlanTaskStatus::Pending),
        ]));
        let reorder = Reorder {
            conversation_id: "conv-1".to_owned(),
            plan_id: "p1".to_owned(),
            number: 2,
            original_order: vec![1, 2],
        };
        let built = list(&conv, Some("p1"), Some(&reorder));
        assert!(built.rows[1].text.starts_with("\u{2195} 2. "));
        assert!(!built.rows[0].text.starts_with('\u{2195}'));
        let other = Reorder {
            conversation_id: "elsewhere".to_owned(),
            ..reorder
        };
        assert!(
            !list(&conv, Some("p1"), Some(&other)).rows[1]
                .text
                .starts_with('\u{2195}'),
            "another conversation's reorder marks nothing"
        );
    }

    #[test]
    fn story21_7_the_list_and_every_excerpt_are_bounded() {
        let many: Vec<PlanTask> = (1..=(MAX_ROWS as u32 + 40))
            .map(|n| task(n, "t", PlanTaskStatus::Pending))
            .collect();
        let built = list(&conversation(plan(many)), Some("p1"), None);
        assert_eq!(built.rows.len(), MAX_ROWS);
        assert!(
            built
                .note
                .unwrap()
                .contains("the first 500 rows are listed")
        );
        let mut big = task(1, "huge", PlanTaskStatus::Completed);
        big.result = Some(TaskResult {
            text: "\u{e9}".repeat(10_000),
            tool_call_count: 0,
            token_count: None,
        });
        let conv = conversation(plan(vec![big]));
        let detail = &list(&conv, Some("p1"), None).rows[0].detail;
        assert!(
            detail.len() < 2_300,
            "excerpted, not cloned whole: {}",
            detail.len()
        );
        assert!(detail.ends_with('\u{2026}'));
    }

    #[test]
    fn story21_7_sync_follows_the_conversation_and_resets_the_selection_for_another_one() {
        let mut state = TuiState::new(80, 24);
        let conv = conversation(plan(vec![
            task(1, "A", PlanTaskStatus::Pending),
            task(2, "B", PlanTaskStatus::Failed),
        ]));
        sync(&mut state, &conv);
        state.mission_control.switch_to(McTab::Tasks);
        state
            .mission_control
            .move_selection(crate::adapters::tui::presentation::viewer::ViewerCmd::LineDown);
        state
            .mission_control
            .move_selection(crate::adapters::tui::presentation::viewer::ViewerCmd::LineDown);
        assert_eq!(
            selected_ref(&state),
            Some(TaskRef {
                conversation_id: "conv-1".to_owned(),
                plan_id: "p1".to_owned(),
                number: 2
            })
        );
        assert_eq!(selected_status(&state), Some(PlanTaskStatus::Failed));
        let rev = state.mission_control.list().unwrap().rev;
        sync(&mut state, &conv);
        assert_eq!(
            state.mission_control.list().unwrap().rev,
            rev,
            "an unchanged conversation rebuilds nothing the caches key on"
        );
        let mut other = conversation(plan(vec![task(1, "Z", PlanTaskStatus::Pending)]));
        other.id = "conv-2".to_owned();
        sync(&mut state, &other);
        assert_eq!(
            selected_ref(&state).map(|task| task.conversation_id),
            Some("conv-2".to_owned()),
            "the old selection is dropped: the default row names the NEW conversation's task"
        );
    }
}
