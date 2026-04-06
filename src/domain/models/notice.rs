/// Severity level for system notices displayed in the status bar.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
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
    /// Short-lived flash message that auto-reverts after expiration.
    Flash { message: String, remaining_ms: u64 },
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

/// Actions available on a feedback block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackAction {
    Retry,
    Compact,
    StartFresh,
    #[allow(dead_code)]
    Dismiss,
    Custom(String),
}

impl FeedbackAction {
    /// Key binding label for display.
    pub fn key_label(&self) -> &str {
        match self {
            FeedbackAction::Retry => "[r] Retry",
            FeedbackAction::Compact => "[c] Compact",
            FeedbackAction::StartFresh => "[n] Start fresh",
            FeedbackAction::Dismiss => "[d] Dismiss",
            FeedbackAction::Custom(label) => label,
        }
    }
}

/// An inline feedback block displayed in the conversation stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackBlock {
    pub id: String,
    pub level: FeedbackLevel,
    pub message: String,
    pub actions: Vec<FeedbackAction>,
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
            StatusState::Flash { message, .. } => message.clone(),
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
