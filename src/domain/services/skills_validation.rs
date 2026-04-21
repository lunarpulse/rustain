//! Stub for `skills-ref-rs` spec-authoritative validation.
//!
//! When the real `skills-ref-rs` crate ships on crates.io, replace
//! this module's body with the actual crate integration.
//! The Cargo.toml feature gate `skills-validation` controls whether
//! this module is compiled.

use std::path::Path;

/// No-op fallback per AC11: returns `Ok(())` unconditionally until
/// `skills-ref-rs` ships on crates.io. Does NOT touch the filesystem —
/// callers must not rely on this for existence/permission checks
/// (those belong to the activator, not the validator).
pub fn validate(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_stub_always_ok() {
        assert!(validate(Path::new("/nonexistent/SKILL.md")).is_ok());
        assert!(validate(Path::new("/")).is_ok());
    }
}
