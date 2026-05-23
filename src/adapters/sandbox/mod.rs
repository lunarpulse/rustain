//! Sandbox adapter module per ADR-06-04 §Decision lines 52-53.
//!
//! Phase A ships `NoOpSandbox` (default; always composable; all platforms)
//! AND `LandlockSandbox` (Linux only, gated on `sandbox` cargo feature).

pub mod noop;
pub use noop::NoOpSandbox;

#[cfg(all(target_os = "linux", feature = "sandbox"))]
pub mod landlock;
#[cfg(all(target_os = "linux", feature = "sandbox"))]
pub use landlock::LandlockSandbox;

use serde::{Deserialize, Serialize};

/// Stable identifier for the active sandbox adapter. Used by logs, telemetry
/// (Story 9.5 status panel), and capability matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxAdapterKind {
    /// `NoOpSandbox` — the default on macOS/Windows, and on Linux without
    /// the `sandbox` cargo feature. Zero OS-level enforcement;
    /// `PermissionChain::check` is the only line of defense.
    NoOp,
    /// `LandlockSandbox` — Linux only, gated on `sandbox` cargo feature.
    /// OS-level enforcement via the Landlock LSM.
    Landlock,
}

/// Errors from sandbox enforcement operations.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// Policy variant not expressible on the current OS/adapter combo.
    #[error("sandbox policy not supported by adapter {adapter:?}: {reason}")]
    Unsupported {
        adapter: SandboxAdapterKind,
        reason: String,
    },
    /// Landlock crate rejected the ruleset (malformed path, kernel ABI too old).
    #[error("landlock ruleset build failed: {0}")]
    RulesetBuildFailed(String),
    /// Kernel ABI is below the Landlock minimum (v3).
    /// NON-FATAL at startup; falls back to NoOpSandbox semantics with a
    /// `tracing::warn!` per ADR-06-04 §Negative.
    #[error("landlock kernel ABI v{found} below minimum v{required}; falling back to NoOp")]
    AbiTooOld { found: u32, required: u32 },
    /// `Command::pre_exec` closure setup failed (rare; typically a borrow or
    /// lifetime issue at composition time, not at runtime).
    #[error("pre_exec setup failed: {0}")]
    PreExecSetupFailed(String),
}
