//! Pinned trust set for minisign signature verification (Story 13.3a, AC2/AC7).
//!
//! These are auditable `const` values — NOT `env!`/`build.rs`-injected.
//! The verifier shape is {keys} × {signatures}: any pair verifies → accept.
//! This is the forward-compat hook for key rotation (dual-sign overlap window).
//!
//! ## Rotation
//! When rotating keys: (1) add the new key to TRUSTED_KEYS, (2) CI dual-signs
//! during the overlap window, (3) remove the old key after all binaries have
//! rolled forward past the overlap. NO runtime trust-update mechanism — the
//! pinned const set IS the feature.

/// Live-release trust root. Empirically verified 2026-06-15 against the real
/// v0.1.0 `SHA256SUMS.minisig` (Task 0.5 spike, 4 assertions GREEN).
///
/// Raw base64, line 2 of `minisign.pub` — no untrusted comment line, no trailing newline.
/// Parsed via `minisign_verify::PublicKey::from_base64`.
const PROD_KEY_1: &str = "RWSUI8k3UrzEelNHSLUobmr5IbvKMx73rDg7gWVBy8vhID/OGxpiJKYF";

// AI-13-3a-2 ship-gate: embed the 2nd backup pubkey before first wide ship.
// The backup key MUST be minted cold/offline. A single-key trust set means
// the first leak is terminal (no signed recovery path).
// const PROD_KEY_BACKUP: &str = "...";

/// The production trust set. In tests, this is overridden via cfg(test).
#[cfg(not(test))]
pub(crate) const TRUSTED_KEYS: &[&str] = &[
    PROD_KEY_1,
    // PROD_KEY_BACKUP — AI-13-3a-2 ship-gate
];

/// Test trust set — mirrors the prod set so the doctor `UpdateHealthCheck`
/// renders the real signing key-id under `cfg(test)` (checks.rs reads this for
/// its key-id display). The verify-module tests pass keys explicitly via the
/// `PROD_KEY`/`STRAY_KEY` consts and never read this; it exists solely for the
/// doctor check's key-id rendering.
#[cfg(test)]
pub(crate) const TRUSTED_KEYS: &[&str] = &[PROD_KEY_1];
