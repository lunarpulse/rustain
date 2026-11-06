//! Identity and borrowed source data for a single tool-call inspector.

use crate::domain::models::{InvocationStatus, PartId, ToolOutput, TurnId};

/// A Stage-bound call identity. Call ids are compared exactly, never parsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolTarget {
    pub conversation_id: String,
    pub tool_call_id: String,
}

/// Stable indices for the frame's four independent body caches and viewers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum ToolTab {
    Output = 0,
    Input = 1,
    Raw = 2,
    Diff = 3,
}

impl ToolTab {
    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Output => "Output",
            Self::Input => "Input",
            Self::Raw => "Raw",
            Self::Diff => "Diff",
        }
    }
}

/// An ephemeral projection: refreshing a frame never clones a tool's output.
///
/// The caller checks the target's conversation id against the active
/// conversation before projecting its turns. Persist the target and rendered
/// body keys in frame state, not this borrow of the conversation/reducer.
#[derive(Debug, Clone, Copy)]
pub struct ToolView<'a> {
    pub target: &'a ToolTarget,
    pub turn_id: &'a TurnId,
    pub part_id: PartId,
    pub name: &'a str,
    pub status: &'a InvocationStatus,
    /// One-based position among invocations in this turn, not among all parts.
    pub ordinal: usize,
    pub args: &'a serde_json::Value,
    /// Joined by the result's `refs`, within the owning turn only.
    pub result: Option<&'a ToolOutput>,
    /// Only the matching invocation in the open turn can borrow a live tail.
    pub tail: Option<&'a str>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// Canonical mutation/replacement epochs, read without traversing payloads.
    pub source_revision: u64,
    pub tail_revision: u64,
}

impl ToolView<'_> {
    pub fn is_running(&self) -> bool {
        matches!(
            self.status,
            InvocationStatus::Pending | InvocationStatus::Running
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_5_tab_indices_are_stable() {
        for (index, tab) in [ToolTab::Output, ToolTab::Input, ToolTab::Raw, ToolTab::Diff]
            .into_iter()
            .enumerate()
        {
            assert_eq!(tab.index(), index);
            assert!(!tab.label().is_empty());
        }
    }
}
