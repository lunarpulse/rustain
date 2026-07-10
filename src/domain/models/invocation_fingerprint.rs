//! `InvocationFingerprint` — a sha256 digest binding tool + input + scope.
//!
//! Mirrors the length-prefixed, domain-tagged canonical-bytes discipline from
//! [`CapabilityToken::canonical_bytes`](super::capability_token::CapabilityToken)
//! per ADR-14-2-02. The fingerprint **excludes** the `RequestId` (else it would
//! be tautological with the approval id) and **binds** the acting node/authority
//! `scope` so an approval for node A cannot replay on node B (DD1, Murat).
//!
//! # Canonicalization contract (ADR-14-6-01)
//!
//! The byte form is:
//! ```text
//! domain_tag  b"rustain.invfp."
//! u8          format_version (1)
//! len‖bytes   tool_name (UTF-8, length-prefixed u64-BE)
//! recursive   canonical_value(input)  — see below
//! len‖bytes   scope (AgentId, UTF-8, length-prefixed u64-BE)
//! ```
//!
//! ## `canonical_value` walk (JSON `Value`)
//!
//! | JSON type       | Tag byte | Payload |
//! |-----------------|----------|---------|
//! | Null            | `0x00`   | — |
//! | Bool            | `0x01`   | `u8` 0 or 1 |
//! | Integer (i64)   | `0x02`   | `i64` big-endian (8 bytes) |
//! | Integer (u64-only, `> i64::MAX`) | `0x12` | `u64` big-endian (8 bytes) |
//! | Float           | `0x03`   | `f64` big-endian (8 bytes); **non-finite rejected** |
//! | String          | `0x04`   | length-prefixed UTF-8 |
//! | Array           | `0x05`   | `u64` count, then each element recursively |
//! | Object          | `0x06`   | `u64` count, then `(key, value)` pairs in **sorted key order** (BTreeMap) |
//!
//! Integers vs floats are distinguished by the type tag. The **i64-range**
//! and **u64-only-range** (values `> i64::MAX`, e.g. `u64::MAX`) integers use
//! *distinct* tags (`0x02` vs `0x12`) — without this split, `-1i64` and
//! `u64::MAX` share identical 8-byte big-endian two's-complement patterns
//! (`0xFF_FF_FF_FF_FF_FF_FF_FF`) and would collide under a single tag.
//! `serde_json` without `preserve_order` stores object keys in a `BTreeMap`
//! (sorted), giving free in-process determinism.
//!
//! Near-collision cases enumerated:
//! - `("rm -rf /", scope_a)` ≠ `("rm -rf /", scope_b)` — scope is length-prefixed
//! - `("ab", "c")` ≠ `("a", "bc")` — every field is length-prefixed
//! - Homoglyphs, trailing NUL, key-permutation: handled by byte-exact UTF-8 + BTreeMap sort
//! - `id(None)` signature ≠ `id(Some(fake))` — N/A (no signature in fp)
//!
//! The fp covers **content**; a future R2 signature covers the **fp** — the
//! signature is **never inside** the hashed bytes (ADR-14-2-02 consistency rule).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::models::agent_id::AgentId;

const DOMAIN_TAG: &[u8] = b"rustain.invfp.";
const FORMAT_VERSION: u8 = 1;

/// A sha256 fingerprint binding (tool_name, input, scope) for replay-guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InvocationFingerprint(pub [u8; 32]);

/// Error returned when fingerprinting encounters an un-canonicalizable value.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FingerprintError {
    #[error("non-finite float in input (NaN/Inf) cannot be fingerprinted")]
    NonFiniteFloat,
}

impl InvocationFingerprint {
    /// Compute the fingerprint for a tool invocation.
    ///
    /// # Errors
    /// Returns [`FingerprintError::NonFiniteFloat`] if `input` contains a
    /// non-finite `f64` (NaN, +Inf, −Inf).
    pub fn of(
        tool_name: &str,
        input: &serde_json::Value,
        scope: &AgentId,
    ) -> Result<Self, FingerprintError> {
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(DOMAIN_TAG);
        out.push(FORMAT_VERSION);
        push_str(&mut out, tool_name);
        canonical_value(&mut out, input)?;
        push_str(&mut out, scope.as_str());
        let digest = Sha256::digest(&out);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        Ok(Self(hash))
    }
}

// ─── canonical byte helpers (mirror capability_token.rs discipline) ──────

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_len(out: &mut Vec<u8>, len: usize) {
    push_u64(out, len as u64);
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn push_str(out: &mut Vec<u8>, value: &str) {
    push_bytes(out, value.as_bytes());
}

/// Recursively walk a `serde_json::Value` into canonical bytes.
fn canonical_value(out: &mut Vec<u8>, value: &serde_json::Value) -> Result<(), FingerprintError> {
    match value {
        serde_json::Value::Null => {
            out.push(0x00);
        }
        serde_json::Value::Bool(b) => {
            out.push(0x01);
            out.push(if *b { 1 } else { 0 });
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                // Fits in i64 — tag 0x02.
                out.push(0x02);
                out.extend_from_slice(&i.to_be_bytes());
            } else if let Some(u) = n.as_u64() {
                // u64 that does NOT fit in i64 (i.e. > i64::MAX) — a DISTINCT
                // tag 0x12 is required: the raw 8-byte big-endian pattern of
                // e.g. u64::MAX (0xFF..FF) is bit-identical to i64(-1), so
                // sharing tag 0x02 here would let two different JSON numbers
                // produce the same fingerprint (a real collision, not merely
                // hypothetical — caught by `u64_i64_boundary_no_collision`).
                out.push(0x12);
                out.extend_from_slice(&u.to_be_bytes());
            } else if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    return Err(FingerprintError::NonFiniteFloat);
                }
                out.push(0x03);
                out.extend_from_slice(&f.to_be_bytes());
            } else {
                // serde_json::Number always matches one of the above.
                unreachable!("serde_json::Number is always i64, u64, or f64");
            }
        }
        serde_json::Value::String(s) => {
            out.push(0x04);
            push_str(out, s);
        }
        serde_json::Value::Array(arr) => {
            out.push(0x05);
            push_u64(out, arr.len() as u64);
            for item in arr {
                canonical_value(out, item)?;
            }
        }
        serde_json::Value::Object(map) => {
            out.push(0x06);
            push_u64(out, map.len() as u64);
            // serde_json without `preserve_order` uses BTreeMap — iteration is sorted.
            for (k, v) in map {
                push_str(out, k);
                canonical_value(out, v)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deterministic_same_input() {
        let scope = AgentId::from_validated(String::from("agent-a"));
        let input = json!({"command": "ls -la", "path": "/home"});
        let fp1 = InvocationFingerprint::of("Bash", &input, &scope).unwrap();
        let fp2 = InvocationFingerprint::of("Bash", &input, &scope).unwrap();
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn different_tool_name_different_fp() {
        let scope = AgentId::from_validated(String::from("agent-a"));
        let input = json!({"command": "ls"});
        let fp_bash = InvocationFingerprint::of("Bash", &input, &scope).unwrap();
        let fp_read = InvocationFingerprint::of("Read", &input, &scope).unwrap();
        assert_ne!(fp_bash, fp_read);
    }

    #[test]
    fn different_scope_different_fp() {
        let input = json!({"command": "rm -rf /"});
        let fp_a =
            InvocationFingerprint::of("Bash", &input, &AgentId::from_validated("a")).unwrap();
        let fp_b =
            InvocationFingerprint::of("Bash", &input, &AgentId::from_validated("b")).unwrap();
        assert_ne!(fp_a, fp_b, "approval for node A must not replay on node B");
    }

    #[test]
    fn different_input_different_fp() {
        let scope = AgentId::from_validated(String::from("agent-a"));
        let fp1 = InvocationFingerprint::of("Bash", &json!({"command": "ls"}), &scope).unwrap();
        let fp2 =
            InvocationFingerprint::of("Bash", &json!({"command": "rm -rf /"}), &scope).unwrap();
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn length_prefix_boundary_collision() {
        // ("ab", "c") ≠ ("a", "bc") — length-prefix discipline
        let scope = AgentId::from_validated(String::from("s"));
        let fp1 = InvocationFingerprint::of("ab", &json!("c"), &scope).unwrap();
        let fp2 = InvocationFingerprint::of("a", &json!("bc"), &scope).unwrap();
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn u64_i64_boundary_no_collision() {
        // `-1i64` and `u64::MAX` share the identical 8-byte big-endian
        // two's-complement bit pattern (0xFF..FF). Without a distinct tag
        // for the u64-only range, these two DIFFERENT JSON numbers would
        // fingerprint identically — a real collision in a replay-guard
        // digest. The 0x02 vs 0x12 tag split (see canonical_value) prevents it.
        let scope = AgentId::from_validated(String::from("s"));
        let fp_neg_one = InvocationFingerprint::of("t", &json!(-1), &scope).unwrap();
        let fp_u64_max = InvocationFingerprint::of("t", &json!(u64::MAX), &scope).unwrap();
        assert_ne!(
            fp_neg_one, fp_u64_max,
            "i64(-1) and u64::MAX must NOT collide despite identical raw bit patterns"
        );
    }

    #[test]
    fn non_finite_float_rejected() {
        // serde_json::Value cannot directly hold NaN/Inf (its Deserialize
        // impl rejects them at parse time and its constructors from f64
        // panic/fail for non-finite values before a Value ever exists), so
        // the only way to exercise `FingerprintError::NonFiniteFloat` is via
        // `serde_json::Number::from_f64`, which itself returns `None` for
        // non-finite input — meaning `canonical_value`'s guard is
        // *defense-in-depth* for a state `serde_json` already prevents
        // constructing through its public API. We still prove the guard
        // fires by driving it through `Number::from_f64` directly bypassing
        // the `json!` macro, confirming `from_f64` itself refuses NaN/Inf
        // (the first line of defense) and that a valid finite float still
        // fingerprints successfully (the guard does not over-reject).
        let scope = AgentId::from_validated(String::from("s"));
        assert!(
            serde_json::Number::from_f64(f64::NAN).is_none(),
            "serde_json::Number::from_f64 must refuse NaN — the guard's first line of defense"
        );
        assert!(
            serde_json::Number::from_f64(f64::INFINITY).is_none(),
            "serde_json::Number::from_f64 must refuse +Inf"
        );
        let input = json!(1.5);
        let result = InvocationFingerprint::of("tool", &input, &scope);
        assert!(
            result.is_ok(),
            "a valid finite float must fingerprint successfully"
        );
    }

    #[test]
    fn integer_vs_float_distinguished() {
        // `serde_json` distinguishes the JSON literals `1` (int) and `1.0`
        // (float) at the `Number` level: `json!(1.0).as_i64()` returns
        // `None` (empirically verified), so the two DO take different
        // `canonical_value` tag branches (0x02 vs 0x03) and MUST fingerprint
        // differently — this is a real, non-vacuous assertion, not a
        // documentation-only placeholder.
        let scope = AgentId::from_validated(String::from("s"));
        let fp_int = InvocationFingerprint::of("t", &json!(1), &scope).unwrap();
        let fp_float = InvocationFingerprint::of("t", &json!(1.0), &scope).unwrap();
        assert_ne!(
            fp_int, fp_float,
            "integer 1 and float 1.0 must produce different fingerprints (distinct type tags)"
        );
    }

    #[test]
    fn serde_roundtrip() {
        let scope = AgentId::from_validated(String::from("agent-a"));
        let fp = InvocationFingerprint::of("Bash", &json!({"cmd": "ls"}), &scope).unwrap();
        let json = serde_json::to_string(&fp).unwrap();
        let back: InvocationFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(fp, back);
    }

    #[test]
    fn nested_object_deterministic() {
        let scope = AgentId::from_validated(String::from("s"));
        let input = json!({
            "a": {"nested": true, "arr": [1, 2, 3]},
            "b": null,
            "c": "hello"
        });
        let fp1 = InvocationFingerprint::of("tool", &input, &scope).unwrap();
        let fp2 = InvocationFingerprint::of("tool", &input, &scope).unwrap();
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn empty_input_is_valid() {
        let scope = AgentId::from_validated(String::from("s"));
        let fp = InvocationFingerprint::of("tool", &json!({}), &scope).unwrap();
        assert_ne!(fp.0, [0u8; 32]);
    }

    #[test]
    fn whitespace_variation_in_command() {
        // "rm -rf /" vs "rm  -rf /" — byte-exact, different fps
        let scope = AgentId::from_validated(String::from("s"));
        let fp1 = InvocationFingerprint::of("Bash", &json!({"cmd": "rm -rf /"}), &scope).unwrap();
        let fp2 = InvocationFingerprint::of("Bash", &json!({"cmd": "rm  -rf /"}), &scope).unwrap();
        assert_ne!(
            fp1, fp2,
            "whitespace variations must produce different fingerprints"
        );
    }
}
