#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
/// Multi-channel communication (TUI, Telegram, Slack, etc.).
///
/// Claudian equivalent: N/A (v1.0+ port, no Claudian counterpart)
pub trait ChannelPort: Send + Sync {
    // v1.0: channel methods
}
