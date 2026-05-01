/// Severity level for system notices displayed in the status bar.
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Key binding label for display (chord-prefix grammar per UX-DR-GLOBAL-CHORD-PREFIX).
    pub fn key_label(&self) -> &str {
        match self {
            FeedbackAction::Retry => "[Ctrl+K r] Retry",
            FeedbackAction::Compact => "[Ctrl+K c] Compact",
            FeedbackAction::StartFresh => "[Ctrl+K n] Start fresh",
            FeedbackAction::Dismiss => "[Ctrl+K x] Dismiss",
            FeedbackAction::Custom(label) => label,
        }
    }

    /// Single source of truth: maps a chord key character to its FeedbackAction.
    /// Co-located with key_label() so round-trip tests can verify parity.
    /// The adapter layer (app.rs) bridges FeedbackAction -> InputAction.
    pub fn dispatch_key(c: char) -> Option<FeedbackAction> {
        match c.to_ascii_lowercase() {
            'r' => Some(FeedbackAction::Retry),
            'c' => Some(FeedbackAction::Compact),
            'n' => Some(FeedbackAction::StartFresh),
            'x' => Some(FeedbackAction::Dismiss),
            _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_label_uses_chord_prefix() {
        assert_eq!(FeedbackAction::Retry.key_label(), "[Ctrl+K r] Retry");
        assert_eq!(FeedbackAction::Compact.key_label(), "[Ctrl+K c] Compact");
        assert_eq!(
            FeedbackAction::StartFresh.key_label(),
            "[Ctrl+K n] Start fresh"
        );
        assert_eq!(FeedbackAction::Dismiss.key_label(), "[Ctrl+K x] Dismiss");
    }

    #[test]
    fn dispatch_key_round_trips_every_variant() {
        let variants = [
            (FeedbackAction::Retry, "[Ctrl+K r] Retry"),
            (FeedbackAction::Compact, "[Ctrl+K c] Compact"),
            (FeedbackAction::StartFresh, "[Ctrl+K n] Start fresh"),
            (FeedbackAction::Dismiss, "[Ctrl+K x] Dismiss"),
        ];

        for (expected_variant, label) in &variants {
            let key_char = label
                .split(']')
                .next()
                .and_then(|s| s.chars().last())
                .expect("label should contain a key character before ']'");

            let action = FeedbackAction::dispatch_key(key_char)
                .unwrap_or_else(|| panic!("dispatch_key('{}') should return Some", key_char));

            assert_eq!(
                action, *expected_variant,
                "dispatch_key('{}') returned {:?}, expected {:?}",
                key_char, action, expected_variant
            );
        }
    }

    #[test]
    fn dispatch_key_unknown_char_returns_none() {
        assert!(FeedbackAction::dispatch_key('z').is_none());
        assert!(FeedbackAction::dispatch_key('y').is_none());
        assert!(FeedbackAction::dispatch_key('a').is_none());
    }
}
