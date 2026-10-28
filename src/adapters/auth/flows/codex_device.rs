//! OpenAI Codex device login: OpenAI's custom device-authorization protocol (not
//! RFC 8628), adapted from oh-my-pi's `loginOpenAICodexDevice`.
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

use super::{
    FlowCtx, FlowError,
    token::{self, Values},
};
use crate::adapters::auth::policies::{CODEX_OAUTH, endpoint};
use crate::domain::models::{Credential, OAuthCredential};
use serde_json::Value;
use std::time::Duration;

const USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const POLL_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
/// The operator's browser visits this URL, so it is displayed verbatim, never rebased.
const VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const FIRST_POLL_CAP_MS: f64 = 5_000.0;
const POLL_SAFETY_MARGIN_MS: f64 = 3_000.0;
const DEFAULT_INTERVAL_S: f64 = 5.0;
const MAX_POLLS: u32 = 120;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(15_000);

#[derive(serde::Serialize)]
struct StartBody<'a> {
    client_id: &'a str,
}
/// Field order is the wire order: `device_auth_id`, then `user_code`.
#[derive(serde::Serialize)]
struct PollBody<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

/// Table A of story 20-1d2: usercode, display, polls, then the exchange. The token
/// request, projection and identity hook are the Codex browser policy's, so the stored
/// row has the browser row's shape and is refreshed by the existing Codex policy.
pub(super) async fn run(ctx: &FlowCtx) -> Result<(Credential, bool), FlowError> {
    let client = token::client_id(CODEX_OAUTH.client_id)?;
    ctx.ui.on_progress("Initiating device authorization…");
    let response = ctx
        .http
        .post(endpoint(USERCODE_URL))
        .timeout(REQUEST_TIMEOUT)
        .json(&StartBody { client_id: &client })
        .send()
        .await
        .map_err(|_| start_unavailable())?;
    let status = response.status();
    if !status.is_success() {
        return Err(failed(format!(
            "Device authorization initiation failed: {}",
            status.as_u16()
        )));
    }
    let start = json_body(response).await.ok_or_else(start_unavailable)?;
    let (Some(device_auth_id), Some(user_code)) = (
        present(&start, "device_auth_id"),
        present(&start, "user_code"),
    ) else {
        return Err(failed(
            "Device authorization response missing required fields",
        ));
    };
    let interval_ms = interval_ms(start.get("interval"));
    ctx.ui.on_device_code(
        VERIFICATION_URL,
        user_code,
        &format!("Enter code: {user_code}"),
    );
    ctx.ui.on_progress(&format!(
        "Waiting for browser authorization (code: {user_code})…"
    ));
    let cancel = ctx.ui.cancel_token();
    for poll in 0..MAX_POLLS {
        (ctx.sleep)(poll_wait(poll, interval_ms)).await;
        // A sleeper may return while cancellation is pending; never send the next poll.
        if cancel.is_cancelled() {
            return Err(FlowError::Cancelled);
        }
        let response = ctx
            .http
            .post(endpoint(POLL_URL))
            .timeout(REQUEST_TIMEOUT)
            .json(&PollBody {
                device_auth_id,
                user_code,
            })
            .send()
            .await
            .map_err(|_| polling_unavailable())?;
        let status = response.status().as_u16();
        // OpenAI answers 403/404 while the operator has not yet authorized.
        if matches!(status, 403 | 404) {
            continue;
        }
        if !response.status().is_success() {
            return Err(failed(format!("Device token polling failed: {status}")));
        }
        let body = json_body(response).await.ok_or_else(polling_unavailable)?;
        let (Some(code), Some(verifier)) = (
            present(&body, "authorization_code"),
            present(&body, "code_verifier"),
        ) else {
            return Err(failed(
                "Device token response missing authorization_code or code_verifier",
            ));
        };
        ctx.ui
            .on_progress("Exchanging authorization code for tokens…");
        return exchange(ctx, &client, code, verifier).await;
    }
    Err(failed(
        "Device authorization timed out — user did not complete login in time",
    ))
}

async fn exchange(
    ctx: &FlowCtx,
    client: &str,
    code: &str,
    verifier: &str,
) -> Result<(Credential, bool), FlowError> {
    let policy = &CODEX_OAUTH;
    let values = Values {
        client_id: client,
        code,
        // The server-issued verifier: this flow generates no PKCE pair of its own.
        verifier,
        redirect: REDIRECT_URI,
        ..Default::default()
    };
    let url = token::resolve_url(&ctx.http, policy.exchange.url)
        .await
        .map_err(FlowError::Message)?;
    let response = token::request(
        &ctx.http,
        policy.exchange,
        &url,
        token::Grant::CodexDeviceExchange,
        &values,
    )
    .await
    .map_err(|e| failed(e.safe_message("Token exchange failed")))?
    .body;
    if present(&response, "access_token").is_none()
        || present(&response, "refresh_token").is_none()
        || !response.get("expires_in").is_some_and(Value::is_number)
    {
        return Err(failed("Token response missing required fields"));
    }
    let now = (ctx.clock)();
    let mut projected = token::project(&response, policy.projection, now)?;
    super::identity::fill(
        &ctx.http,
        policy.identity_hook,
        &projected.access,
        &response,
        &mut projected.identity,
        false,
    )
    .await?;
    Ok((
        Credential::OAuth(OAuthCredential {
            access: projected.access,
            refresh: projected.refresh,
            expires_at_ms: projected.expires_at_ms,
            authorized_at_ms: Some(now),
            identity: projected.identity,
            routing: Default::default(),
        }),
        true,
    ))
}

fn failed(message: impl Into<String>) -> FlowError {
    FlowError::configuration(message)
}

fn start_unavailable() -> FlowError {
    failed("Device authorization initiation failed: transport unavailable")
}

fn polling_unavailable() -> FlowError {
    failed("Device token polling failed: transport unavailable")
}

/// `None` when the body cannot be read; an unparsable body is `Null` (missing fields).
async fn json_body(response: reqwest::Response) -> Option<Value> {
    let text = response.text().await.ok()?;
    Some(serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// omp rejects a falsy field (`!value`): absent, `null` and `""` are missing. Every
/// field read here is a string on the wire, so non-string values are missing too.
fn present<'a>(body: &'a Value, field: &str) -> Option<&'a str> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// omp: `(typeof interval === "number" ? interval
///        : parseInt(String(interval ?? "5"), 10) || 5) * 1000 + 3000`.
fn interval_ms(interval: Option<&Value>) -> f64 {
    let seconds = match interval {
        // Every serde_json number converts without `arbitrary_precision`.
        Some(Value::Number(number)) => number.as_f64().unwrap_or(DEFAULT_INTERVAL_S),
        None | Some(Value::Null) => DEFAULT_INTERVAL_S,
        Some(value) => parse_int(&js_string(value))
            .filter(|seconds| *seconds != 0.0)
            .unwrap_or(DEFAULT_INTERVAL_S),
    };
    seconds * 1000.0 + POLL_SAFETY_MARGIN_MS
}

/// The first poll waits at most 5 s; a non-positive delay does not wait.
fn poll_wait(poll: u32, interval_ms: f64) -> Duration {
    let ms = if poll == 0 {
        interval_ms.min(FIRST_POLL_CAP_MS)
    } else {
        interval_ms
    };
    if ms > 0.0 {
        Duration::try_from_secs_f64(ms / 1000.0).unwrap_or(Duration::MAX)
    } else {
        Duration::ZERO
    }
}

/// JavaScript `String(value)` for non-null JSON values, exact in the prefix `parseInt`
/// reads.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.as_f64().map_or_else(String::new, js_number),
        Value::String(value) => value.clone(),
        // `Array.prototype.join`: `null` elements become empty strings.
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                item => js_string(item),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// JavaScript switches to exponent notation outside `[1e-6, 1e21)`; Rust's shortest
/// round-trip digits match it, so sign and leading digits agree.
fn js_number(value: f64) -> String {
    let magnitude = value.abs();
    if value == 0.0 {
        "0".to_owned()
    } else if !(1e-6..1e21).contains(&magnitude) {
        format!("{value:e}")
    } else {
        value.to_string()
    }
}

/// JavaScript `parseInt(text, 10)`: leading whitespace, an optional sign, then the
/// longest decimal digit prefix; `None` is `NaN`.
fn parse_int(text: &str) -> Option<f64> {
    let text = text.trim_start_matches(is_js_whitespace);
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let end = digits
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(digits.len());
    let value = digits[..end].parse::<f64>().ok()?;
    Some(if negative { -value } else { value })
}

/// ECMAScript `StrWhiteSpaceChar` outside `U+2000..=U+200A`: WhiteSpace (incl. every
/// `Zs`) and LineTerminator.
const JS_WHITESPACE: &str =
    "\t\n\u{b}\u{c}\r \u{a0}\u{1680}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

fn is_js_whitespace(c: char) -> bool {
    ('\u{2000}'..='\u{200a}').contains(&c) || JS_WHITESPACE.contains(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn interval(value: Value) -> f64 {
        interval_ms(Some(&value))
    }

    #[test]
    fn interval_follows_javascript_number_and_parse_int_rules() {
        assert_eq!(interval_ms(None), 8_000.0);
        assert_eq!(interval(Value::Null), 8_000.0);
        // Numbers are taken as-is, including 0, fractions and negatives.
        assert_eq!(interval(json!(0)), 3_000.0);
        assert_eq!(interval(json!(5)), 8_000.0);
        assert_eq!(interval(json!(2.5)), 5_500.0);
        assert_eq!(interval(json!(-10)), -7_000.0);
        // Strings go through parseInt; 0 and NaN fall back to 5.
        assert_eq!(interval(json!("0")), 8_000.0);
        assert_eq!(interval(json!("-0")), 8_000.0);
        assert_eq!(interval(json!("7")), 10_000.0);
        assert_eq!(interval(json!(" \t\n12abc")), 15_000.0);
        assert_eq!(interval(json!("+4")), 7_000.0);
        assert_eq!(interval(json!("-2")), 1_000.0);
        assert_eq!(interval(json!("1e3")), 4_000.0);
        assert_eq!(interval(json!("2.9")), 5_000.0);
        assert_eq!(interval(json!("0x10")), 8_000.0);
        assert_eq!(interval(json!("abc")), 8_000.0);
        assert_eq!(interval(json!("")), 8_000.0);
        assert_eq!(interval(json!("- 3")), 8_000.0);
        assert_eq!(interval(json!("\u{feff}\u{3000}3")), 6_000.0);
        // U+0085 is Unicode whitespace but not ECMAScript whitespace.
        assert_eq!(interval(json!("\u{85}3")), 8_000.0);
        // Other values are stringified first.
        assert_eq!(interval(json!(true)), 8_000.0);
        assert_eq!(interval(json!({"seconds": 9})), 8_000.0);
        assert_eq!(interval(json!([7])), 10_000.0);
        assert_eq!(interval(json!(["4", 9])), 7_000.0);
        assert_eq!(interval(json!([])), 8_000.0);
        assert_eq!(interval(json!([null, 3])), 8_000.0);
        assert_eq!(interval(json!([1e-7])), 4_000.0);
        assert_eq!(interval(json!([2.5e21])), 5_000.0);
        assert_eq!(interval(json!([0.5])), 8_000.0);
    }

    #[test]
    fn first_poll_waits_at_most_five_seconds_and_negative_delays_do_not_wait() {
        assert_eq!(poll_wait(0, 8_000.0), Duration::from_millis(5_000));
        assert_eq!(poll_wait(1, 8_000.0), Duration::from_millis(8_000));
        assert_eq!(poll_wait(119, 8_000.0), Duration::from_millis(8_000));
        assert_eq!(poll_wait(0, 3_000.0), Duration::from_millis(3_000));
        assert_eq!(poll_wait(1, 3_000.0), Duration::from_millis(3_000));
        assert_eq!(poll_wait(0, 5_500.0), Duration::from_millis(5_000));
        assert_eq!(poll_wait(1, 5_500.0), Duration::from_millis(5_500));
        assert_eq!(poll_wait(0, -7_000.0), Duration::ZERO);
        assert_eq!(poll_wait(1, -7_000.0), Duration::ZERO);
        assert_eq!(poll_wait(1, f64::INFINITY), Duration::MAX);
    }

    #[test]
    fn required_fields_follow_javascript_truthiness() {
        let body = json!({"a": "x", "b": "", "c": null, "d": 3});
        assert_eq!(present(&body, "a"), Some("x"));
        assert_eq!(present(&body, "b"), None);
        assert_eq!(present(&body, "c"), None);
        assert_eq!(present(&body, "missing"), None);
        // Whitespace is truthy in JavaScript.
        assert_eq!(present(&json!({"a": " "}), "a"), Some(" "));
        // Non-string values (truthy in JavaScript) are not accepted as wire strings.
        assert_eq!(present(&body, "d"), None);
    }

    #[test]
    fn poll_body_keeps_omp_key_order() {
        let body = serde_json::to_string(&PollBody {
            device_auth_id: "dev",
            user_code: "ABCD-EFGH",
        })
        .unwrap();
        assert_eq!(body, r#"{"device_auth_id":"dev","user_code":"ABCD-EFGH"}"#);
        let start = StartBody {
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
        };
        assert_eq!(
            serde_json::to_string(&start).unwrap(),
            r#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#
        );
    }
}
