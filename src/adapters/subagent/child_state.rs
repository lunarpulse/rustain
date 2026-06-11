use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;

use crate::domain::models::{SubagentRunStatus, ToolPolicy};

/// Per-child runtime state mutated by owner-command Op handlers and read by
/// the child's eventual provider+scheduler loop (Story 10.7).
///
/// All fields use atomic-swap / lock-free types — readers in the child body
/// never block on owner commands. This is the FR50 "absolute authority +
/// immediate effect" contract: an `Op::Pause` flips `paused.store(true)`
/// and the child observes it on the next tool-dispatch boundary without
/// holding any lock.
pub struct ChildState {
    pub paused: Arc<std::sync::atomic::AtomicBool>,
    pub effective_model: Arc<ArcSwap<String>>,
    pub tools_allow: Arc<ArcSwap<ToolPolicy>>,
    pub status: watch::Sender<SubagentRunStatus>,
}

impl ChildState {
    pub fn new(initial_model: String, initial_tools: ToolPolicy) -> Self {
        let (status_tx, _status_rx) = watch::channel(SubagentRunStatus::Idle);
        Self {
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            effective_model: Arc::new(ArcSwap::from_pointee(initial_model)),
            tools_allow: Arc::new(ArcSwap::from_pointee(initial_tools)),
            status: status_tx,
        }
    }
}
