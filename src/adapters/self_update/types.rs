//! Shared types for the self-update subsystem (Story 13.3a).

use std::fmt;

/// These value/error types live in the DOMAIN (`crate::domain::ports::self_update`)
/// so the port traits don't depend on the adapter layer (architecture conformance:
/// domain must not import adapters). Re-exported here so existing
/// `crate::adapters::self_update::types::X` references keep resolving.
pub use crate::domain::ports::self_update::{ReleaseAsset, ReleaseInfo, UpdateError, VerifyError};

/// Single-source-of-truth for the repository coordinates.
pub const GH_OWNER_REPO: &str = "lunarpulse/rustain";

/// Exact-match hosts trusted for download redirects (AC7 channel-pin) — GitHub's
/// API + web origins.
pub const TRUSTED_HOSTS: &[&str] = &["github.com", "api.github.com"];

/// Trusted suffix for GitHub's user-content / release-asset CDN. Every subdomain
/// of `githubusercontent.com` is GitHub-controlled infrastructure, so we match by
/// suffix instead of pinning one CDN host: GitHub rotates them (release assets
/// moved `objects.` → `release-assets.githubusercontent.com`, which broke the
/// old exact allowlist). Suffix-matching `.githubusercontent.com` still rejects
/// look-alikes — `evilgithubusercontent.com` (no leading dot) and
/// `github.com.evil.com` (ends in `.evil.com`) both fail.
pub const TRUSTED_HOST_SUFFIX: &str = ".githubusercontent.com";

/// Maximum binary download size: 64 MB (well above the ~16-30 MB release binaries).
pub const MAX_BINARY_SIZE: usize = 64 * 1024 * 1024;

/// Result of `--check`: always informational, never an error.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckReport {
    pub schema_version: String,
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    /// Human-readable status line (not in JSON).
    #[serde(skip)]
    pub status_line: String,
}

impl fmt::Display for CheckReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.status_line)
    }
}
