use serde::{Deserialize, Serialize};

/// Connection state machine for a single MCP server.
///
/// Transitions:
///   NotConnected → Connecting → Connected
///   Connected → Reconnecting → Connected (on success) or ConnectionFailed (after 5 attempts)
///   Connected → Degraded (tools/list failed after initialize OK)
///   Any → NotConnected (explicit disconnect / shutdown)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum McpConnectionState {
    /// Initial / after explicit disconnect.
    NotConnected,
    /// Active connection attempt in progress.
    Connecting { attempt: u32, started_at_ms: u64 },
    /// Successfully initialized and tools/list cached.
    Connected {
        connected_at_ms: u64,
        tool_count: usize,
    },
    /// Initialize succeeded but tools/list failed; connection kept alive.
    Degraded { since_ms: u64, reason: String },
    /// Scheduled reconnect after disconnect.
    Reconnecting {
        attempt: u32,
        next_retry_in_ms: u64,
        last_error: String,
    },
    /// Max reconnect attempts exhausted; manual intervention required.
    ConnectionFailed { attempts: u32, last_error: String },
    /// Transport is not supported (e.g., SSE per ADR-06-08).
    Unsupported { reason: String },
}

impl Default for McpConnectionState {
    fn default() -> Self {
        Self::NotConnected
    }
}

impl McpConnectionState {
    /// Human-readable health level for status-panel rendering.
    pub fn health_level(&self) -> crate::domain::models::HealthLevel {
        use crate::domain::models::HealthLevel;
        match self {
            McpConnectionState::Connected { .. } => HealthLevel::Healthy,
            McpConnectionState::Degraded { .. } | McpConnectionState::Reconnecting { .. } => {
                HealthLevel::Degraded
            }
            McpConnectionState::NotConnected => HealthLevel::Unknown,
            McpConnectionState::ConnectionFailed { .. }
            | McpConnectionState::Unsupported { .. } => HealthLevel::Error,
            McpConnectionState::Connecting { .. } => HealthLevel::Unknown,
        }
    }

    /// Short metric string for the adapter status panel.
    pub fn metric(&self) -> String {
        match self {
            McpConnectionState::Connected {
                connected_at_ms,
                tool_count,
            } => {
                let age_min = (now_unix() - *connected_at_ms) / 60_000;
                format!("up {age_min}m · tools: {tool_count}")
            }
            McpConnectionState::Degraded { reason, .. } => format!("degraded: {reason}"),
            McpConnectionState::Reconnecting {
                attempt,
                next_retry_in_ms,
                ..
            } => {
                format!("reconnecting {attempt}/5 in {}s", next_retry_in_ms / 1000)
            }
            McpConnectionState::ConnectionFailed {
                attempts,
                last_error,
            } => {
                format!("error: {last_error} ({attempts} attempts)")
            }
            McpConnectionState::Unsupported { reason } => format!("unsupported: {reason}"),
            McpConnectionState::NotConnected => "not connected".to_string(),
            McpConnectionState::Connecting { attempt, .. } => {
                format!("connecting (attempt {attempt})")
            }
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_state_is_not_connected() {
        assert_eq!(
            McpConnectionState::default(),
            McpConnectionState::NotConnected
        );
    }

    #[test]
    fn test_health_level_connected_is_healthy() {
        let s = McpConnectionState::Connected {
            connected_at_ms: 0,
            tool_count: 3,
        };
        assert_eq!(
            s.health_level(),
            crate::domain::models::HealthLevel::Healthy
        );
    }

    #[test]
    fn test_health_level_failed_is_error() {
        let s = McpConnectionState::ConnectionFailed {
            attempts: 5,
            last_error: "boom".into(),
        };
        assert_eq!(s.health_level(), crate::domain::models::HealthLevel::Error);
    }

    #[test]
    fn test_metric_connected() {
        let s = McpConnectionState::Connected {
            connected_at_ms: now_unix(),
            tool_count: 7,
        };
        assert!(s.metric().contains("tools: 7"));
    }
}
