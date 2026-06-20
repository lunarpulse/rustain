//! `SecretString` — a newtype that makes plaintext credential exposure via
//! `derive(Debug)` / `Display` / `Serialize` **structurally unrepresentable**.
//!
//! Story 14.0, AC1.  Hand-rolled (D1 — no `secrecy` crate); concrete (D4 — no
//! generic `Secret<T>` until a real binary-secret consumer lands).
//!
//! # Serde contract (D2)
//!
//! - **`Deserialize`** — yes: raw JSON string → wrapped.
//! - **`Serialize`** — **NO impl at all**.  Accidental egress = compile error.
//!   The single on-disk chokepoint (`auth.json`) uses
//!   `#[serde(serialize_with = "expose_secret_string")]` on the field, which
//!   does NOT require the field type to impl `Serialize`.
//!
//! ```compile_fail
//! // Proves `SecretString` does not implement `Serialize`:
//! let s = rustain::domain::models::SecretString::new("x".into());
//! let _ = serde_json::to_string(&s); // must not compile
//! ```

use serde::{Deserialize, Deserializer, Serializer};
use zeroize::Zeroizing;

/// A credential string that is zeroized on drop and never accidentally printed.
///
/// - `Debug` → `SecretString(<redacted>)`
/// - `Display` → `***`
/// - `Serialize` — not implemented (compile error on accidental egress)
/// - `Deserialize` — wraps a raw string
#[derive(Clone)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    /// Wrap a raw string into a `SecretString`.
    pub fn new(s: String) -> Self {
        Self(Zeroizing::new(s))
    }

    /// The single audited accessor.  Call sites are allowlisted by
    /// `tests/conformance_secret_redaction.rs`.
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        Self::new(s.to_owned())
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretString(<redacted>)")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "***")
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(SecretString::new(s))
    }
}

/// Serialize helper for `#[serde(serialize_with = "expose_secret_string")]`.
///
/// This is the **ONLY** cleartext-to-disk egress — greppable, allowlisted.
/// Does NOT require `SecretString: Serialize`.
pub(crate) fn expose_secret_string<S: Serializer>(
    s: &SecretString,
    ser: S,
) -> Result<S::Ok, S::Error> {
    ser.serialize_str(s.expose_secret())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secretstring_debug_never_exposes_secret() {
        let s = SecretString::new("SECRET-CANARY-DEADBEEF".into());
        let debug = format!("{:?}", s);
        assert!(
            !debug.contains("SECRET-CANARY-DEADBEEF"),
            "Debug leaked the secret: {debug}"
        );
        assert!(
            debug.contains("redacted"),
            "Debug should show masked form: {debug}"
        );
    }

    #[test]
    fn secretstring_display_never_exposes_secret() {
        let s = SecretString::new("SECRET-CANARY-DEADBEEF".into());
        let display = format!("{}", s);
        assert!(
            !display.contains("SECRET-CANARY-DEADBEEF"),
            "Display leaked the secret: {display}"
        );
        assert!(display.contains("***"), "Display should show ***");
    }

    #[test]
    fn expose_secret_returns_real_value() {
        let s = SecretString::new("my-real-key-42".into());
        assert_eq!(s.expose_secret(), "my-real-key-42");
    }

    #[test]
    fn from_string_works() {
        let s: SecretString = "hello".to_string().into();
        assert_eq!(s.expose_secret(), "hello");
    }

    #[test]
    fn clone_preserves_secret() {
        let s = SecretString::new("cloneable".into());
        let c = s.clone();
        assert_eq!(c.expose_secret(), "cloneable");
    }

    #[test]
    fn deserialize_wraps_raw_string() {
        let s: SecretString = serde_json::from_str(r#""sk-test-123""#).unwrap();
        assert_eq!(s.expose_secret(), "sk-test-123");
    }

    #[test]
    fn serialize_with_helper_emits_real_value() {
        #[derive(serde::Serialize)]
        struct Wrapper {
            #[serde(serialize_with = "expose_secret_string")]
            key: SecretString,
        }
        let w = Wrapper {
            key: SecretString::new("real-value".into()),
        };
        let json = serde_json::to_string(&w).unwrap();
        assert!(
            json.contains("real-value"),
            "serialize_with should emit real value"
        );
    }
}
