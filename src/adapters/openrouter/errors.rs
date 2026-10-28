//! OpenRouter HTTP error mapping, shared by the Responses and Chat transports;
//! adapted from oh-my-pi (`ai/src/providers/openai-shared.ts`).
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

/// A final (non-retried) OpenRouter HTTP failure; `request` holds the headers
/// that were sent, for redaction.
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
    let mut message = match error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(message) => redact(message, request),
        // Redact before the cut: no prefix of a bearer straddling it may survive.
        None => redact(body, request).chars().take(500).collect(),
    };
    // omp appends the upstream provider's raw error text.
    if let Some(raw) = error["metadata"]["raw"]
        .as_str()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    {
        message.push('\n');
        message.push_str(&redact(raw, request));
    }
    match status {
        429 => ProviderError::RateLimited {
            retry_after_ms: crate::adapters::responses::retry::hint(
                headers,
                body,
                crate::infrastructure::utils::auth_now_ms(),
            ),
        },
        408 | 500.. => {
            ProviderError::ConnectionFailed(format!("OpenRouter server error {status}: {message}"))
        }
        _ => ProviderError::Other(format!("OpenRouter HTTP {status}: {message}")),
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
            HeaderValue::from_static("Bearer sk-or-v1-secret"),
        );
        headers
    }

    fn map(status: u16, body: &str) -> ProviderError {
        http_error(status, &HeaderMap::new(), body, &sent())
    }

    fn text(error: ProviderError) -> String {
        match error {
            ProviderError::Other(text) | ProviderError::ConnectionFailed(text) => text,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn status_table_and_message_sources() {
        assert!(matches!(
            map(401, r#"{"error":{"message":"No auth"}}"#),
            ProviderError::AuthenticationFailed
        ));
        assert_eq!(
            text(map(402, r#"{"error":{"message":"Insufficient credits"}}"#)),
            "OpenRouter HTTP 402: Insufficient credits"
        );
        assert_eq!(
            text(map(403, r#"{"error":"Key disabled"}"#)),
            "OpenRouter HTTP 403: Key disabled"
        );
        assert_eq!(
            text(map(400, "plain body")),
            "OpenRouter HTTP 400: plain body"
        );
        assert_eq!(
            text(map(503, r#"{"error":{"message":"down"}}"#)),
            "OpenRouter server error 503: down"
        );
        assert_eq!(
            text(map(408, r#"{"error":{"message":"slow"}}"#)),
            "OpenRouter server error 408: slow"
        );
        assert!(matches!(
            map(429, r#"{"error":{"message":"slow down"}}"#),
            ProviderError::RateLimited { .. }
        ));
        assert_eq!(
            text(map(400, &"x".repeat(900))),
            format!("OpenRouter HTTP 400: {}", "x".repeat(500))
        );
    }

    #[test]
    fn upstream_raw_text_is_appended_trimmed_and_blank_raw_is_ignored() {
        assert_eq!(
            text(map(
                402,
                r#"{"error":{"message":"Provider returned error","metadata":{"raw":"  upstream says no \n"}}}"#
            )),
            "OpenRouter HTTP 402: Provider returned error\nupstream says no"
        );
        assert_eq!(
            text(map(
                400,
                r#"{"error":{"message":"Bad","metadata":{"raw":"   "}}}"#
            )),
            "OpenRouter HTTP 400: Bad"
        );
    }

    #[test]
    fn the_sent_bearer_never_survives_in_any_text() {
        for body in [
            r#"{"error":{"message":"bad key sk-or-v1-secret"}}"#,
            r#"{"error":{"message":"m","metadata":{"raw":"echo sk-or-v1-secret"}}}"#,
            "raw body sk-or-v1-secret",
        ] {
            let message = text(map(400, body));
            assert!(!message.contains("sk-or-v1-secret"), "{message}");
            assert!(message.contains("[redacted]"), "{message}");
        }
    }
}
