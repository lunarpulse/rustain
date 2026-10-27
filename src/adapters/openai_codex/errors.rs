//! Codex HTTP errors, adapted from oh-my-pi response-handler.ts.
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

pub(super) fn redact(message: &str, request: &HeaderMap) -> String {
    let mut message = message.to_owned();
    for key in [
        "authorization",
        "chatgpt-account-id",
        "x-openai-internal-codex-residency",
    ] {
        if let Some(value) = request.get(key).and_then(|v| v.to_str().ok()) {
            let value = if key == "authorization" {
                value.strip_prefix("Bearer ").unwrap_or(value)
            } else {
                value
            };
            if !value.is_empty() {
                message = message.replace(value, "[redacted]");
            }
        }
    }
    message
}
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
    let code = error["code"]
        .as_str()
        .or_else(|| error["type"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let now = crate::infrastructure::utils::auth_now_ms();
    if code.contains("usage_limit_reached") || code.contains("usage_not_included") {
        let plan = error["plan_type"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| format!(" ({} plan)", redact(s, request).to_lowercase()))
            .unwrap_or_default();
        let reset = error["resets_at"]
            .as_f64()
            .or_else(|| error["resets_at"].as_str()?.parse().ok())
            .or_else(|| {
                ["x-codex-primary-reset-at", "x-codex-secondary-reset-at"]
                    .iter()
                    .find_map(|key| headers.get(*key)?.to_str().ok()?.parse::<f64>().ok())
            });
        let when = reset
            .filter(|v| v.is_finite() && *v != 0.0)
            .map(|v| {
                format!(
                    " Try again in ~{} min.",
                    ((v * 1000.0 - now as f64) / 60000.0).round().max(0.0) as u64
                )
            })
            .unwrap_or_default();
        return ProviderError::Other(format!(
            "You have hit your ChatGPT usage limit{plan}.{when}"
        ));
    }
    if status == 429 || code == "rate_limit_exceeded" {
        return ProviderError::RateLimited {
            retry_after_ms: crate::adapters::responses::retry::hint(headers, body, now),
        };
    }
    let message = match error["message"].as_str().filter(|s| !s.is_empty()) {
        Some(message) => redact(message, request),
        // Redact before the cut: no prefix of a credential straddling it may survive.
        None => redact(body, request).chars().take(500).collect(),
    };
    if status >= 500 {
        ProviderError::ConnectionFailed(format!("Codex server error {status}: {message}"))
    } else {
        ProviderError::Other(format!("Codex HTTP {status}: {message}"))
    }
}
