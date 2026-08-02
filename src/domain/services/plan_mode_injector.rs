use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::domain::models::{Conversation, MessageRole};

/// Trait for plan-mode reminder injection.
#[async_trait::async_trait]
pub trait PlanModeInjector: Send + Sync {
    /// Called before each turn when Plan mode is active.
    /// Returns `Some(reminder)` when a reminder should be injected into the next user message.
    async fn pre_turn(&self, conv: &Conversation, plan_file: &Path) -> Option<String>;

    /// Reset the re-entry flag so that the next `pre_turn` will fire the re-entry reminder
    /// if the plan file already exists.
    fn reset_reentry(&self);
}

/// Shared, path-free plan-mode nudge for headless `--dry-run` (Story 13.1c, O7).
/// Single source of truth: shares the core posture instruction with the TUI
/// `DefaultPlanInjector::full_reminder` but without path coupling.
pub fn dry_run_reminder() -> &'static str {
    "You are in plan mode (dry-run). Propose a structured plan via the `propose_plan` tool. \
     Do not attempt to modify state — all state-mutating tools will be denied. \
     Use `propose_plan` to describe what you would do, then call `exit_plan_mode` when complete."
}

/// Default implementation of `PlanModeInjector`.
///
/// Reminder cadence (validated against Kimi CLI behavior):
/// - Turn 0 (first assistant turn): full reminder with plan-mode envelope.
/// - Every `reminder_every_n_turns` assistant turns: sparse one-line reminder.
/// - All other turns: `None` (no injection).
/// - Re-entry: once per Plan-mode activation, if the plan file already exists,
///   a re-entry reminder containing the current plan contents is injected.
///
/// The reminder travels to the LLM via `Message.context_prefix`; it is **never**
/// appended to `ChatMessage.content` (which is what the TUI renders and exports).
/// This separation is the core invariant — break it and Plan mode becomes a leaky
/// abstraction.
///
/// The tool schema visible to the model is **never mutated** by this injector.
/// Mode-aware gating happens at the PermissionChain layer, not by adding/removing
/// tools from the toolset.
pub struct DefaultPlanInjector {
    /// How many assistant turns between sparse reminders.
    pub reminder_every_n_turns: u32,
    /// Whether the re-entry reminder has already fired for this activation.
    reentry_fired: AtomicBool,
}

impl DefaultPlanInjector {
    pub fn new() -> Self {
        Self {
            reminder_every_n_turns: 5,
            reentry_fired: AtomicBool::new(false),
        }
    }

    pub fn with_cadence(reminder_every_n_turns: u32) -> Self {
        Self {
            reminder_every_n_turns,
            reentry_fired: AtomicBool::new(false),
        }
    }

    fn full_reminder(&self, plan_file: &Path) -> String {
        format!(
            "Plan mode is active. You MUST NOT make any edits (with the exception of the plan file at {}), run non-readonly tools, or otherwise make changes to the system. Write your plan to the file, then call `exit_plan_mode` when complete.",
            plan_file.display()
        )
    }

    fn sparse_reminder(&self, plan_file: &Path) -> String {
        format!(
            "Reminder: Plan mode is still active. Plan file: {}. Call exit_plan_mode when complete.",
            plan_file.display()
        )
    }
}

impl Default for DefaultPlanInjector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl PlanModeInjector for DefaultPlanInjector {
    async fn pre_turn(&self, conv: &Conversation, plan_file: &Path) -> Option<String> {
        // Re-entry: file exists AND we haven't fired re-entry this activation.
        if plan_file.exists() && !self.reentry_fired.swap(true, Ordering::SeqCst) {
            let contents = tokio::fs::read_to_string(plan_file)
                .await
                .unwrap_or_default();
            return Some(format!(
                "<plan-mode-reentry>\nResuming Plan mode. Existing plan at {}:\n\n{}\n</plan-mode-reentry>",
                plan_file.display(),
                contents,
            ));
        }

        let assistant_turns = conv
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .count();

        match assistant_turns {
            0 => Some(format!(
                "<plan-mode>\n{}\n</plan-mode>",
                self.full_reminder(plan_file)
            )),
            n if (n as u32).is_multiple_of(self.reminder_every_n_turns) => Some(format!(
                "<plan-mode-reminder>\n{}\n</plan-mode-reminder>",
                self.sparse_reminder(plan_file)
            )),
            _ => None,
        }
    }

    fn reset_reentry(&self) {
        self.reentry_fired.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ChatMessage, Conversation, MessageRole};

    fn make_conv(assistant_turns: usize) -> Conversation {
        let mut messages = vec![];
        for _ in 0..assistant_turns {
            messages.push(ChatMessage {
                id: "a".to_string(),
                role: MessageRole::Assistant,
                content: "hi".to_string(),
                content_blocks: vec![],
                tool_calls: vec![],
                created_at: 0,
                token_count: None,
                stop_reason: None,
                synthetic: false,
                images: vec![],
                origin: crate::domain::models::ChannelKind::Terminal,
                authorship: Default::default(),
                retracted_at_ms: None,
            });
        }
        Conversation {
            id: "c1".to_string(),
            title: "t".to_string(),
            messages,
            turns: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
        }
    }

    #[tokio::test]
    async fn turn_0_returns_full_reminder() {
        let tmp = tempfile::tempdir().unwrap();
        let plan_file = tmp.path().join("plan.md");
        let injector = DefaultPlanInjector::new();
        let conv = make_conv(0);
        let reminder = injector.pre_turn(&conv, &plan_file).await;
        assert!(reminder.is_some());
        let r = reminder.unwrap();
        assert!(r.contains("<plan-mode>"));
        assert!(r.contains("Plan mode is active"));
        assert!(r.contains(&plan_file.display().to_string()));
    }

    #[tokio::test]
    async fn turn_5_returns_sparse_reminder() {
        let tmp = tempfile::tempdir().unwrap();
        let plan_file = tmp.path().join("plan.md");
        let injector = DefaultPlanInjector::new();
        let conv = make_conv(5);
        let reminder = injector.pre_turn(&conv, &plan_file).await;
        assert!(reminder.is_some());
        let r = reminder.unwrap();
        assert!(r.contains("<plan-mode-reminder>"));
        assert!(r.contains("Reminder: Plan mode is still active"));
    }

    #[tokio::test]
    async fn turn_3_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let plan_file = tmp.path().join("plan.md");
        let injector = DefaultPlanInjector::new();
        let conv = make_conv(3);
        let reminder = injector.pre_turn(&conv, &plan_file).await;
        assert!(reminder.is_none());
    }

    #[tokio::test]
    async fn reentry_fires_when_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let plan_file = tmp.path().join("plan.md");
        tokio::fs::write(&plan_file, "existing plan").await.unwrap();
        let injector = DefaultPlanInjector::new();
        let conv = make_conv(3); // normally no reminder at turn 3
        let reminder = injector.pre_turn(&conv, &plan_file).await;
        assert!(reminder.is_some());
        let r = reminder.unwrap();
        assert!(r.contains("<plan-mode-reentry>"));
        assert!(r.contains("existing plan"));
    }

    #[tokio::test]
    async fn reentry_fires_only_once() {
        let tmp = tempfile::tempdir().unwrap();
        let plan_file = tmp.path().join("plan.md");
        tokio::fs::write(&plan_file, "existing plan").await.unwrap();
        let injector = DefaultPlanInjector::new();
        let conv = make_conv(3);
        let r1 = injector.pre_turn(&conv, &plan_file).await;
        assert!(r1.is_some());
        // Second call should follow normal cadence (turn 3 -> None)
        let r2 = injector.pre_turn(&conv, &plan_file).await;
        assert!(r2.is_none());
    }

    #[tokio::test]
    async fn reset_reentry_allows_second_firing() {
        let tmp = tempfile::tempdir().unwrap();
        let plan_file = tmp.path().join("plan.md");
        tokio::fs::write(&plan_file, "existing plan").await.unwrap();
        let injector = DefaultPlanInjector::new();
        let conv = make_conv(3);
        let r1 = injector.pre_turn(&conv, &plan_file).await;
        assert!(r1.is_some());
        injector.reset_reentry();
        let r2 = injector.pre_turn(&conv, &plan_file).await;
        assert!(r2.is_some());
    }
}
