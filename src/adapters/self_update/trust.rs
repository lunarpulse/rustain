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

/// Cold/offline RECOVERY key (AI-13-3a-2). Minted offline with C minisign
/// (passwordless, prehashed `ED` — same toolchain as `PROD_KEY_1`, required for
/// CI dual-sign), 2026-06-15. Its SECRET is held in cold/offline storage and as
/// the protected `release`-environment GitHub secret `MINISIGN_SECRET_KEY_BACKUP`
/// — deliberately NOT yet wired into `release.yml`'s signing step (a key not
/// referenced by any workflow can't be abused by a leaked CI token; it is wired
/// in only during an actual rotation/dual-sign window). Embedding the pubkey now
/// is the front-loaded half: field binaries trust it BEFORE any rotation, so a
/// `PROD_KEY_1` compromise has a signed recovery path. Pubkey verified to parse
/// via `PublicKey::from_base64` 2026-06-15.
const PROD_KEY_BACKUP: &str = "RWSZ9j4/sNRoSbgHcslCc6c4kv3n/ILVmb4WdfrWMfWFCqdW3X2IP2na";

/// The production trust set. In tests, this is overridden via cfg(test).
/// Two-member set (AI-13-3a-2): a `{keys}×{sigs}` verify accepts a signature from
/// EITHER key, so adding the backup is additive — no orchestration change.
#[cfg(not(test))]
pub(crate) const TRUSTED_KEYS: &[&str] = &[PROD_KEY_1, PROD_KEY_BACKUP];

/// Test trust set — mirrors the prod set so the doctor `UpdateHealthCheck`
/// renders the real signing key-ids under `cfg(test)` (checks.rs reads this for
/// its key-id display). The verify-module tests pass keys explicitly via the
/// `PROD_KEY`/`STRAY_KEY`/`ATTACKER_KEY` consts and never read this; it exists
/// solely for the doctor check's key-id rendering.
#[cfg(test)]
pub(crate) const TRUSTED_KEYS: &[&str] = &[PROD_KEY_1, PROD_KEY_BACKUP];

#[cfg(test)]
mod tests {
    use super::TRUSTED_KEYS;

    /// Every pinned key MUST parse as a valid minisign public key — guards a
    /// paste that accidentally includes the `untrusted comment:` line, a trailing
    /// newline, or a transcription error (which would silently break verification
    /// of legitimate releases). Also asserts the backup key is present so the
    /// trust set is never silently reduced to a single key (AI-13-3a-2).
    #[test]
    fn all_trusted_keys_parse_and_backup_present() {
        for k in TRUSTED_KEYS {
            assert!(
                minisign_verify::PublicKey::from_base64(k).is_ok(),
                "TRUSTED_KEYS entry failed to parse (malformed paste?): {k}"
            );
        }
        assert!(
            TRUSTED_KEYS.len() >= 2,
            "trust set must keep >=2 members (PROD_KEY_1 + backup) — single-key = terminal on leak"
        );
    }
}
