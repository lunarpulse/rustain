//! Task actions, task-editor parsing and reorder state (Story 6.4, re-keyed by Story 21.7).
//!
//! Every task action arrives with a [`TaskRef`] — the conversation, plan and task number the
//! key was pressed for — and is resolved in **that** conversation (A11, PC-3): [`TaskCtx::admit`]
//! refuses a ref whose conversation is not the active tab's (`Open <title> to change its
//! tasks.`) and every action on a read-only tab (`Tab is read-only`), and a refused action
//! changes nothing and sends nothing. The event loop only calls these functions and runs the
//! async follow-up they return, so a test drives the very code the loop runs.
//!
//! The reorder (A17) lives in the Tasks tab: the state names the conversation, plan and task
//! being moved; a move applied to the plan is undone by [`cancel_reorder`].

use super::event_bus::EventBus;
use crate::adapters::tui::decision_queue::DecisionKind;
use crate::adapters::tui::mission_control::tasks::TaskRef;
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::state::{Direction, Reorder, TuiState};
use crate::adapters::tui::task_panel_handlers::{handle_task_pause, handle_task_skip};
use crate::domain::events::AppEvent;
use crate::domain::models::{
    Conversation, NoticeLevel, PermissionMode, PlanStatus, PlanTaskStatus,
};
use crate::domain::services::peer_text::sanitize_display_line;
use crate::domain::services::plan_runtime::PlanRuntime;

pub fn parse_task_edit(content: &str) -> (String, Vec<String>) {
    let mut title = String::new();
    let mut description = Vec::new();
    let mut reading_desc = false;
    for line in content.lines() {
        if line.starts_with('#') && !reading_desc {
            continue;
        }
        if line == "description: |" {
            reading_desc = true;
        } else if reading_desc {
            if let Some(rest) = line.strip_prefix("  ") {
                description.push(rest.to_string());
            } else if line.is_empty() {
                description.push(String::new());
            } else {
                reading_desc = false;
            }
        } else if let Some(rest) = line.strip_prefix("title: ") {
            title = rest.to_string();
        } else if !line.trim().is_empty() && title.is_empty() {
            title = line.to_string();
        }
    }
    (title, description)
}

/// What the host knows when a task action arrives.
pub struct TaskCtx<'a> {
    pub state: &'a mut TuiState,
    /// The active tab's conversation.
    pub conversation: &'a mut Conversation,
    /// The active tab is a read-only subagent output view.
    pub read_only: bool,
    /// Every open tab (the refusal names the tab that owns the task).
    pub tabs: &'a [crate::domain::models::TabState],
    pub event_bus: &'a EventBus,
}

impl<'a> TaskCtx<'a> {
    /// The context of the event loop: the active tab's flags and every tab's conversation.
    pub fn for_host(
        state: &'a mut TuiState,
        conversation: &'a mut Conversation,
        tab_manager: &'a crate::domain::models::TabManager,
        event_bus: &'a EventBus,
    ) -> Self {
        Self {
            state,
            conversation,
            read_only: tab_manager.active_tab().read_only,
            tabs: tab_manager.tabs(),
            event_bus,
        }
    }
}

/// What the loop runs after a synchronous task action: cancel a running task's token, then
/// advance the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Follow {
    pub plan_id: String,
    pub advance: bool,
    pub cancel_running: Option<u32>,
}

impl TaskCtx<'_> {
    fn notice(&self, level: NoticeLevel, message: impl Into<String>) {
        let _ = self.event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: Some(self.conversation.id.clone()),
            level,
            message: message.into(),
        });
    }

    fn emit_status(&self, plan_id: &str, number: u32) {
        let status = self
            .conversation
            .plans
            .get(plan_id)
            .and_then(|plan| plan.tasks.iter().find(|task| task.number == number))
            .map(|task| task.status);
        if let Some(status) = status {
            let _ = self.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                conversation_id: self.conversation.id.clone(),
                plan_id: plan_id.to_owned(),
                task_number: number,
                status,
            });
        }
    }

    /// Whether the action may run: not on a read-only tab, and only for a task of the active
    /// conversation. A refusal is one Warning and changes nothing.
    pub fn admit(&mut self, task: &TaskRef) -> bool {
        if self.read_only {
            notifications::warning(
                self.state,
                NoticeSource::Tabs,
                super::mission_bridge::READ_ONLY,
            );
        } else if task.conversation_id != self.conversation.id {
            let title = self
                .tabs
                .iter()
                .find(|tab| tab.conversation.id == task.conversation_id)
                .map(|tab| tab.conversation.title.clone())
                .map(|title| sanitize_display_line(&title))
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "that conversation".to_owned());
            notifications::warning(
                self.state,
                NoticeSource::Task,
                format!("Open {title} to change its tasks."),
            );
        } else {
            return true;
        }
        self.state.needs_redraw = true;
        false
    }
}

/// `p` / `s` follow-ups and `r`/`e` retries run the plan on: cancel the running task's token
/// when a running task was paused, then advance.
pub async fn run_follow(
    plan_runtime: &PlanRuntime,
    app_state: &super::app_state::AppState,
    conversation: &mut Conversation,
    mode: PermissionMode,
    follow: Follow,
) {
    if let Some(number) = follow.cancel_running {
        plan_runtime
            .mark_pause_pending(&follow.plan_id, number)
            .await;
        if let Some(snapshot) = plan_runtime.snapshot(&follow.plan_id).await
            && let Some(token) = snapshot.task_cancels.get(&number)
        {
            token.cancel();
        }
    }
    if follow.advance {
        advance(plan_runtime, app_state, conversation, &follow.plan_id, mode).await;
    }
}

/// Advance `plan_id` after a status mutation (pause-resume, skip, retry, edit).
pub async fn advance(
    plan_runtime: &PlanRuntime,
    app_state: &super::app_state::AppState,
    conversation: &mut Conversation,
    plan_id: &str,
    mode: PermissionMode,
) {
    let agents = match super::mission_bridge::get_subagent_provider(&app_state.agent_core) {
        Some(provider) => provider.agent_registry().read().await.agents().to_vec(),
        None => Vec::new(),
    };
    let conversation_id = conversation.id.clone();
    plan_runtime
        .resume_advance(
            &conversation_id,
            plan_id,
            conversation,
            app_state.event_bus.as_ref(),
            &agents,
            mode,
        )
        .await;
}

/// `p`: pause a pending or running task, resume a paused one.
pub fn pause(ctx: &mut TaskCtx<'_>, task: &TaskRef) -> Option<Follow> {
    if !ctx.admit(task) {
        return None;
    }
    let outcome = handle_task_pause(ctx.conversation, &task.plan_id, task.number);
    for notice in &outcome.notices {
        let _ = ctx.event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: Some(notice.conversation_id.clone()),
            level: notice.level,
            message: notice.message.clone(),
        });
    }
    ctx.emit_status(&task.plan_id, task.number);
    ctx.state.needs_redraw = true;
    Some(Follow {
        plan_id: task.plan_id.clone(),
        advance: outcome.should_resume_advance,
        cancel_running: outcome.running_task_paused,
    })
}

/// `s`: skip a pending or failed task; a task others depend on raises the cascade decision
/// (the plan advances only once no cascade decision is waiting).
pub fn skip(ctx: &mut TaskCtx<'_>, task: &TaskRef) -> Option<Follow> {
    if !ctx.admit(task) {
        return None;
    }
    let notices = handle_task_skip(ctx.state, ctx.conversation, &task.plan_id, task.number);
    for notice in &notices {
        let _ = ctx.event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: Some(notice.conversation_id.clone()),
            level: notice.level,
            message: notice.message.clone(),
        });
    }
    ctx.emit_status(&task.plan_id, task.number);
    let conversation_id = ctx.conversation.id.as_str();
    let advance = !ctx.state.decisions.items().any(|item| {
        item.kind == DecisionKind::SkipCascade
            && item.conversation_id.as_deref() == Some(conversation_id)
    });
    ctx.state.needs_redraw = true;
    Some(Follow {
        plan_id: task.plan_id.clone(),
        advance,
        cancel_running: None,
    })
}

/// `r`: put a failed task back to pending and run the plan on.
pub fn retry(ctx: &mut TaskCtx<'_>, task: &TaskRef) -> Option<Follow> {
    if !ctx.admit(task) {
        return None;
    }
    let retried = match ctx.conversation.plans.get_mut(&task.plan_id) {
        Some(plan) => match plan.tasks.iter().position(|t| t.number == task.number) {
            Some(index) if plan.tasks[index].status == PlanTaskStatus::Failed => {
                let target = &mut plan.tasks[index];
                target.status = PlanTaskStatus::Pending;
                target.started_at_ms = None;
                target.completed_at_ms = None;
                target.error = None;
                target.result = None;
                true
            }
            _ => false,
        },
        None => {
            ctx.notice(NoticeLevel::Info, "No active plan.");
            false
        }
    };
    ctx.state.needs_redraw = true;
    if !retried {
        return None;
    }
    let _ = ctx.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
        conversation_id: ctx.conversation.id.clone(),
        plan_id: task.plan_id.clone(),
        task_number: task.number,
        status: PlanTaskStatus::Pending,
    });
    ctx.notice(NoticeLevel::Info, format!("Retrying Task {}.", task.number));
    Some(Follow {
        plan_id: task.plan_id.clone(),
        advance: true,
        cancel_running: None,
    })
}

/// `e`: edit a failed task in the user's editor and queue it again.
pub fn edit(ctx: &mut TaskCtx<'_>, task: &TaskRef) -> Option<Follow> {
    if !ctx.admit(task) {
        return None;
    }
    let number = task.number;
    let failed = ctx
        .conversation
        .plans
        .get(&task.plan_id)
        .and_then(|plan| plan.tasks.iter().find(|t| t.number == number))
        .filter(|t| t.status == PlanTaskStatus::Failed)
        .map(|t| (t.title.clone(), t.description.clone()));
    let (orig_title, orig_description) = failed?;
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let edits_dir = home.join(".rustain").join("edits");
    let _ = std::fs::create_dir_all(&edits_dir);
    let path = edits_dir.join(format!("task-{}-{}.txt", task.plan_id, number));
    let indented: Vec<String> = orig_description
        .lines()
        .map(|line| format!("  {line}"))
        .collect();
    let template = format!(
        "# Edit task title and description below.\n\
         # Save and exit to apply; close without changes to cancel.\n\
         # Lines starting with # are ignored.\n\
         title: {}\n\
         description: |\n{}",
        orig_title.replace('\n', " "),
        indented.join("\n")
    );
    let _ = std::fs::write(&path, &template);
    // The editor helper suspends and restores the terminal itself.
    let exit = crate::adapters::tui::editor_suspend::run_editor_on_path(&path);
    ctx.state.needs_redraw = true;
    if exit.is_err() {
        ctx.notice(
            NoticeLevel::Info,
            "Task edit cancelled (editor exited with error).",
        );
        return None;
    }
    let Ok(content) = std::fs::read_to_string(&path) else {
        ctx.notice(
            NoticeLevel::Warning,
            "Failed to read edited file. Task unchanged.",
        );
        return None;
    };
    if content == template {
        ctx.notice(NoticeLevel::Info, "Task edit cancelled.");
        return None;
    }
    let (new_title, new_description) = parse_task_edit(&content);
    if new_title.is_empty() {
        ctx.notice(
            NoticeLevel::Warning,
            "Failed to parse edited task — missing title. Task unchanged.",
        );
        return None;
    }
    if let Some(target) = ctx
        .conversation
        .plans
        .get_mut(&task.plan_id)
        .and_then(|plan| plan.tasks.iter_mut().find(|t| t.number == number))
    {
        target.title = new_title;
        target.description = new_description.join("\n");
        target.status = PlanTaskStatus::Pending;
        target.started_at_ms = None;
        target.completed_at_ms = None;
        target.error = None;
        target.result = None;
    }
    let _ = ctx.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
        conversation_id: ctx.conversation.id.clone(),
        plan_id: task.plan_id.clone(),
        task_number: number,
        status: PlanTaskStatus::Pending,
    });
    ctx.notice(
        NoticeLevel::Info,
        format!("Task {number} edited and queued for retry."),
    );
    Some(Follow {
        plan_id: task.plan_id.clone(),
        advance: true,
        cancel_running: None,
    })
}

/// `x` on the Tasks tab: ask to cancel the plan the ref names (a decision; nothing is
/// cancelled until it is confirmed).
pub fn cancel_plan(ctx: &mut TaskCtx<'_>, task: &TaskRef) {
    if !ctx.admit(task) {
        return;
    }
    let executing = ctx
        .conversation
        .plans
        .get(&task.plan_id)
        .is_some_and(|plan| plan.status == PlanStatus::Executing);
    if executing {
        crate::adapters::tui::handlers::plan_decisions::arrive_cancel_plan_confirmation(
            ctx.state,
            ctx.conversation,
            task.plan_id.clone(),
        );
    } else {
        ctx.notice(NoticeLevel::Info, "No active plan to cancel.");
    }
    ctx.state.needs_redraw = true;
}

/// Palette `!cancel-plan`: ask to cancel the active conversation's executing plan.
pub fn cancel_active_plan(ctx: &mut TaskCtx<'_>) {
    let plan_id = ctx
        .conversation
        .plans
        .values()
        .find(|plan| plan.status == PlanStatus::Executing)
        .map(|plan| plan.id.clone());
    let Some(plan_id) = plan_id else {
        ctx.notice(NoticeLevel::Info, "No active plan to cancel.");
        ctx.state.needs_redraw = true;
        return;
    };
    let task = TaskRef {
        conversation_id: ctx.conversation.id.clone(),
        plan_id,
        number: 0,
    };
    cancel_plan(ctx, &task);
}

/// Palette `!reorder-task` over the Tasks tab's selection: start moving a pending task.
/// Returns whether a reorder started.
pub fn enter_reorder(ctx: &mut TaskCtx<'_>, task: &TaskRef) -> bool {
    if !ctx.admit(task) {
        return false;
    }
    let started = ctx
        .conversation
        .plans
        .get(&task.plan_id)
        .filter(|plan| {
            plan.tasks
                .iter()
                .any(|t| t.number == task.number && t.status == PlanTaskStatus::Pending)
        })
        .map(|plan| Reorder {
            conversation_id: ctx.conversation.id.clone(),
            plan_id: plan.id.clone(),
            number: task.number,
            original_order: plan.tasks.iter().map(|t| t.number).collect(),
        });
    match started {
        Some(reorder) => ctx.state.task_panel_state.reorder = Some(reorder),
        None => ctx.notice(
            NoticeLevel::Warning,
            "Reorder requires a pending task selected.",
        ),
    }
    ctx.state.needs_redraw = true;
    ctx.state.task_panel_state.reorder.is_some()
}

/// The reorder in progress, when it belongs to `conversation`.
fn active_reorder<'a>(state: &'a TuiState, conversation: &Conversation) -> Option<&'a Reorder> {
    state
        .task_panel_state
        .reorder
        .as_ref()
        .filter(|reorder| reorder.conversation_id == conversation.id)
}

pub fn move_reorder(
    state: &mut TuiState,
    conversation: &mut Conversation,
    direction: Direction,
    event_bus: &EventBus,
) -> bool {
    let Some((plan_id, number)) = active_reorder(state, conversation)
        .map(|reorder| (reorder.plan_id.clone(), reorder.number))
    else {
        return false;
    };
    if let Some(plan) = conversation.plans.get_mut(&plan_id)
        && let Some(index) = plan.tasks.iter().position(|task| task.number == number)
    {
        let new_index = match direction {
            Direction::Up => index.saturating_sub(1),
            Direction::Down => {
                if index + 1 < plan.tasks.len() {
                    index + 1
                } else {
                    index
                }
            }
            Direction::Left | Direction::Right => index,
        };
        if index == new_index {
            return false;
        }
        if let Err(reason) =
            crate::domain::services::plan_runtime::validate_reorder(plan, number, new_index)
        {
            let _ = event_bus.emit_domain(AppEvent::SystemNotice {
                conversation_id: Some(conversation.id.clone()),
                level: NoticeLevel::Warning,
                message: format!("Reorder violates dependencies: {}.", reason),
            });
        } else {
            let task = plan.tasks.remove(index);
            plan.tasks.insert(new_index, task);
        }
    }
    state.needs_redraw = true;
    true
}

pub fn commit_reorder(state: &mut TuiState, conversation: &Conversation, event_bus: &EventBus) {
    if let Some(reorder) = active_reorder(state, conversation)
        && let Some(plan) = conversation.plans.get(&reorder.plan_id)
    {
        for task in &plan.tasks {
            let _ = event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                conversation_id: conversation.id.clone(),
                plan_id: reorder.plan_id.clone(),
                task_number: task.number,
                status: PlanTaskStatus::Pending,
            });
        }
    }
    state.task_panel_state.reorder = None;
    let _ = event_bus.emit_domain(AppEvent::SystemNotice {
        conversation_id: Some(conversation.id.clone()),
        level: NoticeLevel::Info,
        message: "Plan reordered.".to_string(),
    });
    state.needs_redraw = true;
}

/// Put the plan's tasks back in `original` order (tasks added since stay at the end).
fn restore_order(plan: &mut crate::domain::models::Plan, original: Vec<u32>) {
    let mut restored = Vec::with_capacity(plan.tasks.len());
    for number in original {
        if let Some(index) = plan.tasks.iter().position(|task| task.number == number) {
            restored.push(plan.tasks.remove(index));
        }
    }
    restored.append(&mut plan.tasks);
    plan.tasks = restored;
}

pub fn cancel_reorder(state: &mut TuiState, conversation: &mut Conversation) {
    if let Some(reorder) = state.task_panel_state.reorder.take()
        && reorder.conversation_id == conversation.id
        && let Some(plan) = conversation.plans.get_mut(&reorder.plan_id)
    {
        restore_order(plan, reorder.original_order);
    }
    state.task_panel_state.reorder = None;
    state.needs_redraw = true;
}

/// A reorder belongs to the conversation it was armed in. When the active conversation is
/// another one (a decision switched tabs), the provisional order is rolled back in its own
/// tab and the reorder is dropped, so it can never capture the other tab's keys or be
/// committed or cancelled there (7D-03). Returns whether a reorder was settled.
pub fn settle_reorder_origin(
    state: &mut TuiState,
    active: &Conversation,
    tabs: &mut [crate::domain::models::TabState],
) -> bool {
    if !state
        .task_panel_state
        .reorder
        .as_ref()
        .is_some_and(|reorder| reorder.conversation_id != active.id)
    {
        return false;
    }
    let Some(reorder) = state.task_panel_state.reorder.take() else {
        return false;
    };
    let restored = tabs
        .iter_mut()
        .find(|tab| tab.conversation.id == reorder.conversation_id)
        .and_then(|tab| tab.conversation.plans.get_mut(&reorder.plan_id))
        .map(|plan| restore_order(plan, reorder.original_order))
        .is_some();
    notifications::info(
        state,
        NoticeSource::Task,
        if restored {
            "Reorder cancelled: the tab changed; the original task order was restored."
        } else {
            "Reorder cancelled: its tab is gone."
        },
    );
    state.needs_redraw = true;
    true
}
