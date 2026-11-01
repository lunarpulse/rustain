/// Severity level for system notices displayed in the status bar.
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeLevel {
    Info,
    /// Warning-class AND turn-fatal: the consumer clears streaming state and
    /// aborts the active turn. Use when the turn cannot usefully continue.
    Warning,
    /// Warning-class but **NOT** turn-fatal: rendered exactly like `Warning`
    /// (a persistent warning row, not a transient flash) while the turn keeps
    /// running. Use to DISCLOSE something about a turn that is still valid —
    /// e.g. a declared tool restriction this build cannot honour (FR42-a).
    ///
    /// ⚠ Story 19.2 code review: routing a disclosure through `Warning` made
    /// the notice cancel the very turn it described, because the consumer
    /// (`event_loop.rs`) aborts `_active_turn` for every non-`Info` notice.
    /// Any new notice that is informative-but-not-fatal belongs here.
    Advisory,
    Error,
}

impl NoticeLevel {
    /// Whether a notice of this level ENDS the turn in progress: the TUI
    /// consumer clears streaming state and aborts the active turn handle.
    ///
    /// Story 19.2 code review made this a named predicate instead of an inline
    /// `matches!` duplicated at two consumer sites — a disclosure that fired
    /// mid-turn was cancelling the turn it described, and nothing tested the
    /// rule.
    pub fn is_turn_fatal(self) -> bool {
        matches!(self, NoticeLevel::Warning | NoticeLevel::Error)
    }
}

/// State for tracking retry attempts with exponential backoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryState {
    pub attempt: u8,
    pub max_attempts: u8,
    pub delay_ms: u64,
}

/// Calculate exponential backoff delay for a given attempt (0-indexed).
/// Returns 1000, 2000, 4000, 8000, 16000 ms.
pub fn next_delay(attempt: u8) -> u64 {
    1000 * (1u64 << attempt.min(4))
}

/// Typed status state replacing the fragile `status_message: String` pattern.
/// Each variant carries the data needed for status bar rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusState {
    /// No activity — default state.
    Idle,
    /// Provider is streaming a response.
    Streaming,
    /// A tool is executing.
    Executing { tool_name: String, elapsed_ms: u64 },
    /// Retrying after provider error with exponential backoff.
    Retrying {
        attempt: u8,
        max: u8,
        next_in_ms: u64,
    },
}

impl Default for StatusState {
    fn default() -> Self {
        Self::Idle
    }
}

/// Severity level for feedback blocks displayed inline in conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackLevel {
    Error,
    Warning,
    Info,
}

/// An inline feedback block displayed in the conversation stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackBlock {
    pub id: String,
    pub level: FeedbackLevel,
    pub message: String,
    /// Insertion sequence (Story 21.1 S2, A10): the view shown last has the
    /// highest `seq` in its map and renders last. Assigned by `show_view` only.
    pub seq: u64,
}

/// Story 19.16g — the seen-through boundary one transparency-log view would
/// contribute **once it is actually presented**.
///
/// Bound to the exact snapshot the view was read from (`seen_through` is
/// that report's maximum row `seq`) and to the local reset revision in effect
/// when it was read — never to a later observed head. It travels with the
/// view that can be presented (the panel, or the `team-log` block and its
/// tab) and is consumed once. A local reminder boundary: not an
/// acknowledgement, not a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogVisitCandidate {
    pub seen_through: u64,
    pub reset_revision: u64,
}

impl StatusState {
    /// Render the status state as a display string for the status bar.
    pub fn display_text(&self) -> String {
        match self {
            StatusState::Idle => "Ready".to_string(),
            StatusState::Streaming => "Streaming...".to_string(),
            StatusState::Executing { tool_name, .. } => {
                format!("Executing {}...", tool_name)
            }
            StatusState::Retrying {
                attempt,
                max,
                next_in_ms,
            } => {
                format!(
                    "Retrying ({}/{}) in {:.1}s",
                    attempt,
                    max,
                    *next_in_ms as f64 / 1000.0
                )
            }
        }
    }

    /// Whether this state represents active work (for streaming color).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            StatusState::Streaming | StatusState::Executing { .. } | StatusState::Retrying { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Story 19.2 code review — the structural ratchet for the defect that the
    /// review found: a disclosure notice emitted mid-turn was aborting the very
    /// turn it described, because the consumer treated EVERY non-`Info` notice
    /// as terminal. These pin which levels may end a turn.
    #[test]
    fn advisory_notices_never_end_the_turn() {
        assert!(
            !NoticeLevel::Advisory.is_turn_fatal(),
            "an Advisory disclosure must leave the turn running"
        );
        assert!(!NoticeLevel::Info.is_turn_fatal());
    }

    #[test]
    fn warning_and_error_notices_end_the_turn() {
        // The pre-existing "filters are disjoint" notice relies on this, and so
        // does the model-fallback path that re-submits after aborting.
        assert!(NoticeLevel::Warning.is_turn_fatal());
        assert!(NoticeLevel::Error.is_turn_fatal());
    }
}
