use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;

use crate::domain::models::{AgentMetrics, NodeState, ToolPolicy};

pub fn tool_policy_summary(policy: &ToolPolicy) -> String {
    fn summarise(prefix: &str, tools: &std::collections::BTreeSet<String>) -> String {
        if tools.is_empty() {
            return format!("{prefix}: none");
        }
        let preview: Vec<String> = tools.iter().take(3).cloned().collect();
        let suffix = if tools.len() > 3 {
            format!(" (+{} more)", tools.len() - 3)
        } else {
            String::new()
        };
        format!("{prefix}: {}{}", preview.join(", "), suffix)
    }

    match policy {
        ToolPolicy::InheritFromParent => "inherit".to_string(),
        ToolPolicy::Allowlist { tools } => summarise("allow", tools),
        ToolPolicy::Denylist { tools } => summarise("deny", tools),
    }
}

/// Per-child runtime state mutated by owner-command Op handlers and read by
/// the child's eventual provider+scheduler loop (Story 10.7 / 14.1 AC11).
///
/// All fields use atomic-swap / watch types — readers in the child body never
/// block on owner commands. This is the FR50 "absolute authority + immediate
/// effect" contract: an `Op::Pause` flips `paused.store(true)` and the child
/// observes it on the next tool-dispatch boundary without holding any lock.
pub struct ChildState {
    pub paused: Arc<std::sync::atomic::AtomicBool>,
    pub effective_model: Arc<ArcSwap<String>>,
    pub tools_allow: Arc<ArcSwap<ToolPolicy>>,
    pub status: watch::Sender<NodeState>,
    pub metrics: watch::Sender<AgentMetrics>,
}

impl ChildState {
    pub fn new(initial_model: String, initial_tools: ToolPolicy) -> Self {
        let (status_tx, _status_rx) = watch::channel(NodeState::Created);
        let initial_metrics = AgentMetrics {
            effective_model: initial_model.clone(),
            tools_summary: tool_policy_summary(&initial_tools),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
        };
        let (metrics_tx, _metrics_rx) = watch::channel(initial_metrics);
        Self {
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            effective_model: Arc::new(ArcSwap::from_pointee(initial_model)),
            tools_allow: Arc::new(ArcSwap::from_pointee(initial_tools)),
            status: status_tx,
            metrics: metrics_tx,
        }
    }

    pub fn current_metrics(&self) -> AgentMetrics {
        self.metrics.borrow().clone()
    }

    pub fn update_metrics(&self, f: impl FnOnce(&mut AgentMetrics)) {
        let mut next = self.metrics.borrow().clone();
        f(&mut next);
        let _ = self.metrics.send(next);
    }
}
