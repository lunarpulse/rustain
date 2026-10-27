//! Unverified JWT payload decoding, adapted from oh-my-pi's Codex identity hook.
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};

/// Decode claims for identity/profile selection, NOT authentication or verification.
/// Both Node-compatible base64 alphabets and optional padding are accepted.
pub fn claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    // Node also ignores non-zero trailing bits in the final symbol.
    const CONFIG: GeneralPurposeConfig = GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true);
    let alphabet = if payload.bytes().any(|byte| matches!(byte, b'-' | b'_')) {
        &alphabet::URL_SAFE
    } else {
        &alphabet::STANDARD
    };
    let decoded = GeneralPurpose::new(alphabet, CONFIG).decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_and_url_safe_payloads_with_optional_padding() {
        let value = serde_json::json!("\u{ffff}");
        let payload = serde_json::to_vec(&value).unwrap();
        let standard = base64::engine::general_purpose::STANDARD.encode(&payload);
        let url_safe = base64::engine::general_purpose::URL_SAFE.encode(&payload);
        assert_ne!(standard, url_safe, "vector must exercise both alphabets");
        for encoded in [&standard, &url_safe] {
            assert_eq!(
                claims(&format!("header.{encoded}.signature")),
                Some(value.clone())
            );
            assert_eq!(
                claims(&format!(
                    "header.{}.signature",
                    encoded.trim_end_matches('=')
                )),
                Some(value.clone())
            );
        }
    }

    #[test]
    fn non_canonical_trailing_bits_decode_like_node() {
        assert_eq!(claims("a.e31.c"), Some(serde_json::json!({})));
    }

    #[test]
    fn malformed_tokens_have_no_claims() {
        for token in [
            "",
            "one",
            "one.two",
            "a.b.c.d",
            "a.%%%!!.c",
            "a..c",
            "a.bm90LWpzb24.c",
            "a./w==.c",
        ] {
            assert!(
                claims(token).is_none(),
                "malformed token unexpectedly decoded"
            );
        }
    }
}
