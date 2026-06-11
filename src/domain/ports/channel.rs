#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
/// Multi-channel communication (TUI, Telegram, Slack, etc.).
///
/// Claudian equivalent: N/A (v1.0+ port, no Claudian counterpart)
// 2026-05-17 — Story 8.4 added shutdown_loop/start_loop (cold tier)
// following the additive-with-defaults discipline. No existing adapters needed changes.
// 2026-05-19 — Story 8.5 added health_snapshot() with default HealthSummary::unknown() impl
// following additive-with-defaults discipline. No existing adapters needed changes.
// Real metrics ship with real adapters in Epic 12.
#[async_trait::async_trait]
pub trait ChannelPort: Send + Sync {
    // v1.0: channel methods
    fn health_snapshot(&self) -> crate::domain::models::HealthSummary {
        crate::domain::models::HealthSummary::unknown()
    }
    async fn shutdown_loop(&self) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }
    async fn start_loop(&self) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }
    /// Push an unsolicited message to the channel's configured destination (for
    /// example a cron result forward). Default is no-op for terminal/noop
    /// channels. Story 12.4 AC3.
    async fn notify(&self, _text: &str) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }
}
