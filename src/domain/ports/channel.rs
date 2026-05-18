#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
/// Multi-channel communication (TUI, Telegram, Slack, etc.).
///
/// Claudian equivalent: N/A (v1.0+ port, no Claudian counterpart)
// 2026-05-17 — Story 8.4 added shutdown_loop/start_loop (cold tier)
// following the additive-with-defaults discipline. No existing adapters needed changes.
#[async_trait::async_trait]
pub trait ChannelPort: Send + Sync {
    // v1.0: channel methods
    async fn shutdown_loop(&self) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }
    async fn start_loop(&self) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }
}
