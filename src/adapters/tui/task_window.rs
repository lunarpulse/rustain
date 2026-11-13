//! The task window (Story 21.7 A8, AC17): one task's facts and result in a frame of its own,
//! opened by `Enter` on a Tasks row. One window at a time (a global slot like the agent and
//! artifact windows): opening another task retargets it.
//!
//! It is the artifact window's text window with a task as its subject: the facts (the old
//! drill-down's header), then the result through `markdown_body::layout` at the frame's inner
//! width (Rendered by default; `v` Source; `/` line filter; RU-25 scrolling). An error, a skip
//! reason and a cancel reason are shown literally (Source), never as markdown. What is the
//! task's own: `r` retries a failed task, `s` skips a pending or failed one, `e` edits a failed
//! one, `p` pauses or resumes — each painted only where the task's status allows it — and `c`
//! copies the result, error or skip reason with its own success wording (DF-277).
//!
//! Every action names its task by a [`TaskRef`] taken from the window (`conversation`, `plan`,
//! `number`), never from whatever is active when the key is handled: the host refuses a ref
//! whose conversation is not the active writable tab's (A11). The window is **stage-bound**: its
//! frame carries its conversation, so a reset, a close or a tab switch of that Stage pops it.
//!
//! It follows its task: every sync ([`refresh`]) re-reads the facts and the result; when the
//! task is gone the window keeps its last copy under `This task is no longer available.` and
//! `[Esc] Back` (the stale copy of RU-9).

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::artifact_window::{
    self as text, ArtifactBody, ArtifactWindowState, Slot,
};
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::{CommandId, Context, Host, Key};
use crate::adapters::tui::mission_control::tasks::TaskRef;
use crate::adapters::tui::presentation::markdown_body::BodyMode;
use crate::adapters::tui::presentation::{Surface, reducer};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::task_panel_handlers::resolve_copy_task_payload;
use crate::adapters::tui::widgets::task_detail;
use crate::domain::models::Conversation;
use crate::domain::models::plan::PlanTaskStatus;
use crate::domain::services::peer_text::sanitize_display_line;

/// The line a vanished task's window opens with.
pub const GONE: &str = "This task is no longer available.";

pub struct TaskWindowState {
    conversation_id: String,
    pub plan_id: String,
    pub number: u32,
    /// The facts and the result: the artifact window's text window.
    pub text: ArtifactWindowState,
    /// The task as it was last read (what the stale copy keeps saying).
    status: PlanTaskStatus,
    task_title: String,
    facts: String,
    /// The task left the plan (or the plan left the conversation).
    pub gone: bool,
    /// The fingerprint of the task as last read (see [`window_fp`]).
    fp: Option<u64>,
}

impl std::fmt::Debug for TaskWindowState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskWindowState")
            .field("conversation_id", &self.conversation_id)
            .field("plan_id", &self.plan_id)
            .field("number", &self.number)
            .field("gone", &self.gone)
            .finish_non_exhaustive()
    }
}

/// What the window shows for a task as it is now.
struct Read {
    facts: String,
    body: Vec<u8>,
    status: PlanTaskStatus,
    title: String,
    copy_done: String,
}

fn read(conversation: &Conversation, plan_id: &str, number: u32) -> Option<Read> {
    let plan = conversation.plans.get(plan_id)?;
    let task = plan.tasks.iter().find(|task| task.number == number)?;
    let payload = resolve_copy_task_payload(conversation, Some(plan_id), number, None)?;
    Some(Read {
        facts: task_detail::facts(plan, task),
        body: payload.text.into_bytes(),
        status: task.status,
        title: task.title.clone(),
        copy_done: payload.flash_message,
    })
}

impl TaskWindowState {
    /// A window on task `number` of `plan_id` in `conversation`; `None` when the task is not
    /// there.
    #[must_use]
    pub fn new(conversation: &Conversation, plan_id: &str, number: u32) -> Option<Self> {
        let read = read(conversation, plan_id, number)?;
        let mut text = ArtifactWindowState::new(
            format!("{plan_id}#{number}"),
            read.facts.clone(),
            ArtifactBody::Source(read.body),
        );
        text.set_literal(is_literal(read.status));
        text.set_copy_done(read.copy_done);
        Some(Self {
            conversation_id: conversation.id.clone(),
            plan_id: plan_id.to_owned(),
            number,
            text,
            status: read.status,
            fp: window_fp(conversation, plan_id, number),
            task_title: read.title,
            facts: read.facts,
            gone: false,
        })
    }

    /// The conversation whose Stage owns the window.
    #[must_use]
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// The task this window acts on.
    #[must_use]
    pub fn task_ref(&self) -> TaskRef {
        TaskRef {
            conversation_id: self.conversation_id.clone(),
            plan_id: self.plan_id.clone(),
            number: self.number,
        }
    }

    /// The status the window last read (the rail's offers follow it).
    #[must_use]
    pub fn status(&self) -> PlanTaskStatus {
        self.status
    }

    /// Follow the task: new facts and result, or the stale copy once it is gone. A task that
    /// did not change since the last read ([`window_fp`]) costs a hash of its scalars and
    /// copies nothing, so a repaint never re-clones a large result (7D-02).
    pub fn follow(&mut self, conversation: &Conversation) {
        let fp = window_fp(conversation, &self.plan_id, self.number);
        if fp.is_some() && !self.gone && fp == self.fp {
            return;
        }
        match read(conversation, &self.plan_id, self.number) {
            Some(read) => {
                self.gone = false;
                self.fp = fp;
                self.status = read.status;
                self.task_title = read.title;
                self.facts = read.facts.clone();
                self.text.set_literal(is_literal(read.status));
                self.text.set_copy_done(read.copy_done);
                self.text
                    .replace(read.facts, ArtifactBody::Source(read.body));
            }
            None => {
                if !self.gone {
                    self.gone = true;
                    let body = self.text.body.clone();
                    self.text.replace(format!("{GONE}\n{}", self.facts), body);
                }
            }
        }
    }

    /// The frame title, `Task <n> · <title>` (the title is untrusted: sanitized here).
    #[must_use]
    pub fn title(&self) -> String {
        format!(
            "Task {} \u{B7} {}",
            self.number,
            sanitize_display_line(&self.task_title)
        )
    }
}

/// A result is markdown; an error, a skip reason, a cancel reason and the status line of a
/// task that has not finished are literal: shown as Source and never offered as markdown
/// (7D-04, A8).
fn is_literal(status: PlanTaskStatus) -> bool {
    status != PlanTaskStatus::Completed
}

/// What a window shows for a task depends on: hashed from borrowed scalars (no copy of any
/// result or error text), `None` when the task is not there.
fn window_fp(conversation: &Conversation, plan_id: &str, number: u32) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let plan = conversation.plans.get(plan_id)?;
    let task = plan.tasks.iter().find(|task| task.number == number)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    plan.title.hash(&mut hasher);
    crate::adapters::tui::mission_control::tasks::hash_task(task, &mut hasher);
    Some(hasher.finish())
}

fn get(state: &TuiState) -> Option<&ArtifactWindowState> {
    state.task_window.as_ref().map(|window| &window.text)
}

fn get_mut(state: &mut TuiState) -> Option<&mut ArtifactWindowState> {
    state.task_window.as_mut().map(|window| &mut window.text)
}

pub(crate) const SLOT: Slot = Slot {
    surface: Surface::TaskWindow,
    context: Context::TaskWindow,
    copy: MissionCmd::CopyResult,
    noun: "task result",
    get,
    get_mut,
};

/// Open (or retarget) the task window on `task` from the active conversation. `false` when the
/// task is not in it (nothing opens).
pub fn open(state: &mut TuiState, conversation: &Conversation, task: &TaskRef) -> bool {
    if task.conversation_id != conversation.id {
        return false;
    }
    let Some(window) = TaskWindowState::new(conversation, &task.plan_id, task.number) else {
        return false;
    };
    let title = window.title();
    reducer::open_task_window(state, window, title);
    true
}

/// The sync before a paint (or after a task action): the open window follows its task when the
/// active conversation is its own (a frame of another Stage is already gone), and a retitled
/// task retitles the frame.
pub fn refresh(state: &mut TuiState, conversation: &Conversation) {
    let Some(window) = state.task_window.as_mut() else {
        return;
    };
    if window.conversation_id != conversation.id {
        return;
    }
    window.follow(conversation);
    let title = window.title();
    if let Some(frame) = state
        .modal_stack
        .all_frames_mut()
        .find(|frame| frame.surface == Surface::TaskWindow)
    {
        frame.tool_title = Some(title);
    }
}

/// The task window is the top frame.
#[must_use]
pub fn is_top(state: &TuiState) -> bool {
    text::is_top_in(state, &SLOT)
}

/// The filter field of the window is open (a text context of the frame).
#[must_use]
pub fn search_editing(state: &TuiState) -> bool {
    text::search_editing_in(state, &SLOT)
}

pub(crate) fn on_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    text::on_key_in(state, key, host, &SLOT, &mut |state, command| {
        let CommandId::Mission(command) = command else {
            return None;
        };
        let window = state.task_window.as_ref()?;
        let live = window.text.live();
        let task = window.task_ref();
        let (allowed, make): (bool, fn(TaskRef) -> InputAction) = match command {
            MissionCmd::Pause => (live.pause, InputAction::TaskPause),
            MissionCmd::Skip => (live.skip, InputAction::TaskSkip),
            MissionCmd::Retry => (live.retry, InputAction::TaskRetry),
            MissionCmd::Edit => (live.edit, InputAction::TaskEdit),
            _ => return None,
        };
        Some(if allowed {
            make(task)
        } else {
            InputAction::Consumed
        })
    })
}

pub(crate) fn on_wheel(
    state: &mut TuiState,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    text::on_wheel_in(state, &SLOT, delta);
}

pub(crate) fn search_changed(state: &mut TuiState) {
    text::search_changed_in(state, &SLOT);
}

pub(crate) fn accept_search(state: &mut TuiState) {
    text::accept_search_in(state, &SLOT);
}

pub(crate) fn clear_search(state: &mut TuiState) {
    text::clear_search_in(state, &SLOT);
}

pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    text::ignores_raw_key_in(state, &SLOT, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::plan::{Plan, PlanStatus, PlanTask, TaskResult};

    fn conversation(
        status: PlanTaskStatus,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Conversation {
        let task = PlanTask {
            number: 1,
            title: "Compile".to_owned(),
            description: String::new(),
            depends_on: vec![],
            status,
            started_at_ms: None,
            completed_at_ms: None,
            result: result.map(|text| TaskResult {
                text: text.to_owned(),
                tool_call_count: 2,
                token_count: None,
            }),
            error: error.map(str::to_owned),
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        };
        let plan = Plan {
            id: "p1".to_owned(),
            title: "Ship".to_owned(),
            tasks: vec![task],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 1,
            resolved_at: None,
            host_message_id: None,
            origin_restrictions: None,
        };
        Conversation {
            id: "conv-1".to_owned(),
            plans: std::collections::HashMap::from([("p1".to_owned(), plan)]),
            ..Conversation::default()
        }
    }

    #[test]
    fn story21_7_a_result_is_rendered_and_an_error_is_literal_and_both_copy_their_own_text() {
        let done = conversation(PlanTaskStatus::Completed, Some("**bold** ok"), None);
        let window = TaskWindowState::new(&done, "p1", 1).unwrap();
        assert_eq!(window.text.mode, BodyMode::Rendered);
        assert_eq!(
            window.text.body,
            ArtifactBody::Source(b"**bold** ok".to_vec())
        );
        assert_eq!(window.title(), "Task 1 \u{B7} Compile");
        let failed = conversation(PlanTaskStatus::Failed, None, Some("**not markdown**"));
        let window = TaskWindowState::new(&failed, "p1", 1).unwrap();
        assert_eq!(window.text.mode, BodyMode::Source);
        assert_eq!(
            window.text.body,
            ArtifactBody::Source(b"**not markdown**".to_vec())
        );
        assert!(TaskWindowState::new(&failed, "p1", 9).is_none());
        assert!(TaskWindowState::new(&failed, "nope", 1).is_none());
    }

    #[test]
    fn story21_7_follow_tracks_the_status_and_a_vanished_task_keeps_its_last_copy() {
        let mut conv = conversation(PlanTaskStatus::Running, None, None);
        let mut window = TaskWindowState::new(&conv, "p1", 1).unwrap();
        assert_eq!(window.status(), PlanTaskStatus::Running);
        {
            let task = &mut conv.plans.get_mut("p1").unwrap().tasks[0];
            task.status = PlanTaskStatus::Completed;
            task.result = Some(TaskResult {
                text: "finished".to_owned(),
                tool_call_count: 0,
                token_count: None,
            });
        }
        window.follow(&conv);
        assert_eq!(window.status(), PlanTaskStatus::Completed);
        assert_eq!(window.text.mode, BodyMode::Rendered, "a result renders");
        assert_eq!(window.text.body, ArtifactBody::Source(b"finished".to_vec()));
        conv.plans.clear();
        window.follow(&conv);
        assert!(window.gone);
        assert_eq!(
            window.text.body,
            ArtifactBody::Source(b"finished".to_vec()),
            "the stale copy keeps the last result"
        );
        assert_eq!(window.title(), "Task 1 \u{B7} Compile");
    }

    #[test]
    fn story21_7_a_cancelled_task_window_is_literal_and_copies_the_cancel_reason() {
        let cancelled = conversation(
            PlanTaskStatus::Cancelled,
            None,
            Some("cancelled by the **user**"),
        );
        let window = TaskWindowState::new(&cancelled, "p1", 1).unwrap();
        assert_eq!(window.title(), "Task 1 \u{B7} Compile");
        assert_eq!(window.status(), PlanTaskStatus::Cancelled);
        assert_eq!(
            window.text.mode,
            BodyMode::Source,
            "a cancel reason is literal"
        );
        assert_eq!(
            window.text.body,
            ArtifactBody::Source(b"cancelled by the **user**".to_vec())
        );
        // The copy wording the window carries is the one the handler resolves.
        let payload = resolve_copy_task_payload(&cancelled, Some("p1"), 1, None).unwrap();
        assert_eq!(payload.flash_message, "Copied cancel reason to clipboard.");
        assert_eq!(payload.text, "cancelled by the **user**");
        // Copying from the open window says so (the success notice drops the full stop).
        let mut state = TuiState::new(160, 40);
        let copied = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let sink = copied.clone();
        state.clipboard_copier = Some(std::sync::Arc::new(move |text| {
            sink.lock().push(text.to_owned());
            crate::adapters::tui::clipboard::ClipboardResult::Osc52Success
        }));
        let task = TaskRef {
            conversation_id: "conv-1".to_owned(),
            plan_id: "p1".to_owned(),
            number: 1,
        };
        assert!(open(&mut state, &cancelled, &task));
        crate::adapters::tui::artifact_window::copy_in(&mut state, &SLOT);
        assert_eq!(*copied.lock(), ["cancelled by the **user**"]);
        assert!(
            state
                .notifications
                .records()
                .any(|record| record.message == "Copied cancel reason to clipboard"),
            "{:?}",
            state
                .notifications
                .records()
                .map(|r| r.message.clone())
                .collect::<Vec<_>>()
        );
    }
}
