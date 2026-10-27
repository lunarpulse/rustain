//! xAI HTTP errors, adapted from oh-my-pi (ai/src/error/rate-limit.ts,
//! openai-responses.ts error handling).
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

use crate::domain::errors::ProviderError;
use reqwest::header::HeaderMap;
use serde_json::Value;

/// A 403 carrying one of these is xAI's account credit cap, not a bad credential
/// (omp `rate-limit.ts` usage-limit texts).
const CREDIT_CAP: [&str; 7] = [
    "exhausted",
    "quota",
    "usage limit",
    "run out of credits",
    "out of credits",
    "spending-limit",
    "spending limit",
];

/// Removes the sent bearer from provider-supplied text.
pub(super) fn redact(message: &str, request: &HeaderMap) -> String {
    let bearer = request
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
        .filter(|v| !v.is_empty());
    match bearer {
        Some(bearer) => message.replace(bearer, "[redacted]"),
        None => message.to_owned(),
    }
}

/// A non-retried xAI HTTP failure. The transport has already spent the one
/// stored-OAuth recovery a 401 allows.
pub(super) fn http_error(
    status: u16,
    headers: &HeaderMap,
    body: &str,
    request: &HeaderMap,
) -> ProviderError {
    if status == 401 {
        return ProviderError::AuthenticationFailed;
    }
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = &value["error"];
    let message = match error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(message) => redact(message, request),
        // Redact before the cut: no prefix of a bearer straddling it may survive.
        None => redact(body, request).chars().take(500).collect(),
    };
    if status == 403 {
        let lower = message.to_lowercase();
        return if CREDIT_CAP.iter().any(|text| lower.contains(text)) {
            ProviderError::Other(format!("xAI HTTP 403: {message}"))
        } else {
            ProviderError::AuthenticationFailed
        };
    }
    let code = error["code"]
        .as_str()
        .or_else(|| error["type"].as_str())
        .or_else(|| value["code"].as_str())
        .unwrap_or("");
    if status == 429 || code.eq_ignore_ascii_case("rate_limit_exceeded") {
        return ProviderError::RateLimited {
            retry_after_ms: crate::adapters::responses::retry::hint(
                headers,
                body,
                crate::infrastructure::utils::auth_now_ms(),
            ),
        };
    }
    if status >= 500 {
        ProviderError::ConnectionFailed(format!("xAI server error {status}: {message}"))
    } else {
        ProviderError::Other(format!("xAI HTTP {status}: {message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn sent() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer xai-secret"),
        );
        headers
    }

    fn map(status: u16, body: &str) -> ProviderError {
        http_error(status, &HeaderMap::new(), body, &sent())
    }

    #[test]
    fn status_table_matches_the_provider_contract() {
        assert!(matches!(
            map(401, "{}"),
            ProviderError::AuthenticationFailed
        ));
        assert!(matches!(
            map(403, r#"{"error":{"message":"Forbidden"}}"#),
            ProviderError::AuthenticationFailed
        ));
        for text in [
            "Your credits are EXHAUSTED",
            "monthly spending-limit reached",
            "You have run out of credits",
        ] {
            let body = serde_json::json!({ "code": "x", "error": text }).to_string();
            match map(403, &body) {
                ProviderError::Other(message) => {
                    assert_eq!(message, format!("xAI HTTP 403: {text}"))
                }
                other => panic!("credit cap mapped to {other:?}"),
            }
        }
        let mut retry = HeaderMap::new();
        retry.insert("retry-after", HeaderValue::from_static("2"));
        assert!(matches!(
            http_error(429, &retry, "{}", &sent()),
            ProviderError::RateLimited {
                retry_after_ms: Some(2_000)
            }
        ));
        assert!(matches!(
            map(
                400,
                r#"{"error":{"code":"rate_limit_exceeded","message":"slow"}}"#
            ),
            ProviderError::RateLimited { .. }
        ));
        match map(503, r#"{"error":{"message":"down"}}"#) {
            ProviderError::ConnectionFailed(message) => {
                assert_eq!(message, "xAI server error 503: down");
            }
            other => panic!("5xx mapped to {other:?}"),
        }
        match map(402, &"x".repeat(600)) {
            ProviderError::Other(message) => {
                assert_eq!(message, format!("xAI HTTP 402: {}", "x".repeat(500)));
            }
            other => panic!("402 mapped to {other:?}"),
        }
    }

    #[test]
    fn provider_text_never_echoes_the_bearer() {
        match map(400, r#"{"error":{"message":"bad key xai-secret"}}"#) {
            ProviderError::Other(message) => {
                assert_eq!(message, "xAI HTTP 400: bad key [redacted]");
            }
            other => panic!("400 mapped to {other:?}"),
        }
        let straddling = format!("{}xai-secret", "x".repeat(495));
        match map(400, &straddling) {
            ProviderError::Other(message) => assert!(!message.contains("xai-s"), "{message}"),
            other => panic!("400 mapped to {other:?}"),
        }
    }
}
