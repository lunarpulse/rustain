//! Cloud Code Assist HTTP error mapping, adapted from oh-my-pi
//! (ai/src/providers/google-gemini-cli.ts, ai/src/utils/google-validation.ts).
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

use crate::adapters::auth::gemini_cli_project::{validation_message, validation_url};
use crate::domain::errors::ProviderError;
use reqwest::header::HeaderMap;
use serde_json::Value;

/// Removes the bearer the request carried from provider-supplied text.
pub(super) fn redact(message: &str, request: &HeaderMap) -> String {
    let token = request
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.strip_prefix("Bearer ").unwrap_or(value))
        .filter(|token| !token.is_empty());
    match token {
        Some(token) => message.replace(token, "[redacted]"),
        None => message.to_owned(),
    }
}

/// The final non-2xx of a logical request. `email` is the identity of the grant
/// that was sent.
pub(super) fn http_error(
    status: u16,
    headers: &HeaderMap,
    body: &str,
    request: &HeaderMap,
    email: Option<&str>,
) -> ProviderError {
    if status == 401 {
        return ProviderError::AuthenticationFailed;
    }
    if let Some(url) = validation_url(body) {
        return ProviderError::Other(validation_message(&url, "retry your request", email));
    }
    if status == 429 {
        return ProviderError::RateLimited {
            retry_after_ms: crate::adapters::responses::retry::hint(
                headers,
                body,
                crate::infrastructure::utils::auth_now_ms(),
            ),
        };
    }
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let message = match value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
    {
        Some(message) => redact(message, request),
        // Redact before the cut: no prefix of a credential straddling it may survive.
        None => redact(body, request).chars().take(500).collect(),
    };
    let text = format!("Cloud Code Assist API error ({status}): {message}");
    if status >= 500 {
        ProviderError::ConnectionFailed(text)
    } else {
        ProviderError::Other(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn sent() -> HeaderMap {
        let mut headers = HeaderMap::new();
        let mut bearer = HeaderValue::from_static("Bearer BEARER-CANARY");
        bearer.set_sensitive(true);
        headers.insert("authorization", bearer);
        headers
    }

    fn map(status: u16, body: &str) -> ProviderError {
        http_error(
            status,
            &HeaderMap::new(),
            body,
            &sent(),
            Some("me@example.com"),
        )
    }

    #[test]
    fn statuses_map_to_the_provider_contract() {
        assert!(matches!(map(401, ""), ProviderError::AuthenticationFailed));
        assert!(matches!(
            map(429, "Please retry in 2s"),
            ProviderError::RateLimited {
                retry_after_ms: Some(2000)
            }
        ));
        match map(503, r#"{"error":{"message":"overloaded"}}"#) {
            ProviderError::ConnectionFailed(text) => {
                assert_eq!(text, "Cloud Code Assist API error (503): overloaded");
            }
            other => panic!("{other:?}"),
        }
        match map(403, r#"{"error":{"message":"denied"}}"#) {
            ProviderError::Other(text) => {
                assert_eq!(text, "Cloud Code Assist API error (403): denied");
            }
            other => panic!("{other:?}"),
        }
        let long = "x".repeat(600);
        match map(400, &long) {
            ProviderError::Other(text) => {
                assert_eq!(
                    text,
                    format!("Cloud Code Assist API error (400): {}", "x".repeat(500))
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn validation_bodies_name_the_account_and_never_echo_the_bearer() {
        let body = r#"{"error":{"details":[{"reason":"VALIDATION_REQUIRED","metadata":{"validation_url":"https://accounts.google.com/verify"}}]}}"#;
        match map(403, body) {
            ProviderError::Other(text) => assert_eq!(
                text,
                "Account verification required for me@example.com. Visit https://accounts.google.com/verify to continue, then retry your request."
            ),
            other => panic!("{other:?}"),
        }
        let insecure = body.replace("https://", "http://");
        match map(403, &insecure) {
            ProviderError::Other(text) => {
                assert!(
                    text.starts_with("Cloud Code Assist API error (403): "),
                    "{text}"
                );
            }
            other => panic!("{other:?}"),
        }
        match map(400, r#"{"error":{"message":"bad token BEARER-CANARY"}}"#) {
            ProviderError::Other(text) => {
                assert_eq!(
                    text,
                    "Cloud Code Assist API error (400): bad token [redacted]"
                );
            }
            other => panic!("{other:?}"),
        }
        match map(500, "echo BEARER-CANARY") {
            ProviderError::ConnectionFailed(text) => assert!(!text.contains("BEARER-CANARY")),
            other => panic!("{other:?}"),
        }
    }
}
