//! Sandbox adapter module (ADR-06-04, amended by ADR-23-1-01).
//!
//! `NoOpSandbox` (always composable; all platforms) and `LandlockSandbox`
//! (Linux only, `sandbox` cargo feature, part of `default`). The composition
//! root picks one through `infrastructure::sandbox::resolve_sandbox`.

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
    /// `NoOpSandbox` — zero OS-level enforcement;
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
    /// The tool call fails; nothing is spawned unconfined.
    #[error("landlock ruleset build failed: {0}")]
    RulesetBuildFailed(String),
}
