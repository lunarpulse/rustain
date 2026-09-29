//! Exponential-backoff reconnect task for MCP servers.

use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::adapters::mcp::client::McpClientAdapter;

/// Maximum total initial-connect attempts, including `lazy_connect_all`'s
/// immediate front-door call.
pub const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Backoff before retry `retry` (1-based): **1s, 2s, 4s, 8s**.
///
/// The immediate call is attempt 1. Four delayed retries keep the complete
/// production path inside the five-attempt contract; treating this constant as
/// "five more retries" produced six dials and a final `attempts: 6` state.
#[must_use]
pub fn backoff_for_attempt(retry: u32) -> Duration {
    Duration::from_millis(1000 * 2u64.pow(retry.saturating_sub(1)))
}

/// Spawn a reconnect task for a single MCP client.
///
/// Owns the ONLY retry loop on the initial-connect path — which is why the rmcp
/// Streamable HTTP transport is pinned to `NeverRetry` (Story 9.9 AC4): two
/// retry owners stacked would multiply the envelope invisibly.
///
/// ⛔ This is initial-connect retry only. There is **no** mid-session disconnect
/// detection for any transport: `spawn_reconnect_task` has exactly one call
/// site (`lazy_connect.rs`, inside startup's `lazy_connect_all`), and
/// `McpConnectionState::Reconnecting` is never constructed anywhere. Do not
/// write "reconnects on connection loss" (Story 9.9 ruling A6,
/// `DF-9-9-NO-LIVE-DISCONNECT-DETECT`).
pub fn spawn_reconnect_task(client: Arc<McpClientAdapter>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let max_attempts: u32 = MAX_RECONNECT_ATTEMPTS;
        let ct = client.cancel_token();

        // `lazy_connect_all` already made attempt 1. Retry only attempts 2..=5.
        for attempt in 2..=max_attempts {
            if ct.is_cancelled() {
                return;
            }

            let backoff = backoff_for_attempt(attempt - 1);
            tokio::select! {
                _ = sleep(backoff) => {},
                _ = ct.cancelled() => return,
            }

            match client.connect().await {
                Ok(()) => {
                    tracing::info!(
                        server = %client.server_id(),
                        "MCP server reconnected (attempt {attempt}/{max_attempts})"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        server = %client.server_id(),
                        error = %e,
                        "MCP server reconnect attempt {attempt}/{max_attempts} failed"
                    );
                }
            }
        }

        tracing::error!(
            server = %client.server_id(),
            "MCP server connection failed after {max_attempts} attempts. Use /mcp reconnect <name> to retry (Story 9.2) or restart rustain."
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production envelope is one immediate call plus four delayed retries.
    /// Raising the total cap or restoring a fifth retry turns this RED without
    /// a wall-clock assertion.
    #[test]
    fn the_retry_envelope_is_five_total_attempts() {
        assert_eq!(MAX_RECONNECT_ATTEMPTS, 5);
        let schedule: Vec<u64> = (1..MAX_RECONNECT_ATTEMPTS)
            .map(|retry| backoff_for_attempt(retry).as_secs())
            .collect();
        assert_eq!(schedule, vec![1, 2, 4, 8]);
        assert_eq!(1 + schedule.len(), MAX_RECONNECT_ATTEMPTS as usize);
        assert_eq!(schedule.iter().sum::<u64>(), 15);
    }
}
