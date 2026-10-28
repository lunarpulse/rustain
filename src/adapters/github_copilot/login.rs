//! GitHub Copilot device login: Enterprise prompt, device code, polling with
//! omp's cadence, routing probe and model-policy enablement, adapted from
//! oh-my-pi (ai/src/registry/oauth/github-copilot.ts).
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

use std::time::Duration;

use super::{domain, policy, routing};
use crate::adapters::auth::flows::{FlowCtx, FlowError, safe_error_id};
use crate::adapters::auth::policies::{COPILOT_GRANT_LIFETIME_MS, endpoint};
use crate::domain::models::{
    Credential, CredentialIdentity, CredentialRouting, OAuthCredential, SecretString,
};

/// omp's rendered Enterprise prompt.
pub const PROMPT: &str = "GitHub Enterprise URL/domain (blank for github.com) (company.ghe.com): ";
const USER_AGENT: &str = "copilot-developer-action/0.0.1";
const SCOPE: &str = "read:user";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Each start and poll request is bounded (omp: unbounded).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MINIMUM_INTERVAL_MS: u64 = 1000;
const SLOW_DOWN_INCREMENT_MS: u64 = 5000;
const TIMEOUT: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";

struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: f64,
    expires_in: f64,
}

/// The poll cadence: the interval and its multiplier in tenths (1.2 at first,
/// 1.4 after a `slow_down`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cadence {
    interval_ms: u64,
    tenths: u64,
}

impl Cadence {
    fn new(interval_seconds: f64) -> Self {
        Self {
            interval_ms: whole_ms(interval_seconds).max(MINIMUM_INTERVAL_MS),
            tenths: 12,
        }
    }
    /// `ceil(interval_ms × multiplier)`, exact in integer milliseconds.
    fn wait_ms(self) -> u64 {
        (self.interval_ms * self.tenths).div_ceil(10)
    }
    /// A positive `interval` replaces the interval, else 5 s are added; the
    /// multiplier becomes 1.4.
    fn slow_down(&mut self, interval: Option<f64>) {
        self.interval_ms = match interval.filter(|seconds| *seconds > 0.0) {
            Some(seconds) => whole_ms(seconds),
            None => self.interval_ms + SLOW_DOWN_INCREMENT_MS,
        }
        .max(MINIMUM_INTERVAL_MS);
        self.tenths = 14;
    }
}

fn whole_ms(seconds: f64) -> u64 {
    let ms = (seconds * 1000.0).floor();
    if ms.is_finite() && ms > 0.0 {
        ms.min(u64::MAX as f64 / 16.0) as u64
    } else {
        0
    }
}

fn form(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish()
}

fn post(ctx: &FlowCtx, url: String, body: String) -> reqwest::RequestBuilder {
    ctx.http()
        .post(endpoint(&url))
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("user-agent", USER_AGENT)
        .body(body)
        .timeout(REQUEST_TIMEOUT)
}

/// A status followed only by a whitelisted protocol id; bodies are never echoed.
fn status_detail(status: u16, body: &str) -> String {
    match safe_error_id(status, body) {
        Some(id) => format!("{status} {id}"),
        None => status.to_string(),
    }
}

fn cancelled(ctx: &FlowCtx) -> Result<(), FlowError> {
    if ctx.ui.cancel_token().is_cancelled() {
        Err(FlowError::Cancelled)
    } else {
        Ok(())
    }
}

pub async fn run(ctx: &FlowCtx) -> Result<(Credential, bool), FlowError> {
    let answer = ctx.ui.prompt_line(PROMPT).await.unwrap_or_default();
    cancelled(ctx)?;
    let login = domain::login_domain(&answer)
        .ok_or_else(|| FlowError::Message("Invalid GitHub Enterprise URL/domain".into()))?;
    let client_id = login.client_id();
    let device = start(ctx, &login.oauth_host, client_id).await?;
    ctx.ui.on_device_code(
        &device.verification_uri,
        &device.user_code,
        &format!("Enter code: {}", device.user_code),
    );
    let token = poll(ctx, &login.oauth_host, client_id, &device).await?;
    let api_endpoint = routing::probe(ctx.http(), &token).await;
    ctx.ui.on_progress("Enabling models...");
    policy::enable_all(
        ctx.http(),
        &token,
        api_endpoint.as_deref(),
        login.enterprise.as_deref(),
    )
    .await;
    let now = (ctx.clock)();
    Ok((
        Credential::OAuth(OAuthCredential {
            access: SecretString::new(token.clone()),
            refresh: SecretString::new(token),
            expires_at_ms: now + COPILOT_GRANT_LIFETIME_MS,
            authorized_at_ms: Some(now),
            identity: CredentialIdentity::default(),
            routing: CredentialRouting {
                enterprise_domain: login.enterprise,
                api_endpoint,
            },
        }),
        true,
    ))
}

async fn start(ctx: &FlowCtx, host: &str, client_id: &str) -> Result<DeviceCode, FlowError> {
    let failed = |detail: String| {
        FlowError::Message(format!(
            "github-copilot device authorization failed: {detail}"
        ))
    };
    let response = post(
        ctx,
        format!("https://{host}/login/device/code"),
        form(&[("client_id", client_id), ("scope", SCOPE)]),
    )
    .send()
    .await
    .map_err(|_| failed("transport unavailable".into()))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|_| failed("transport unavailable".into()))?;
    if !(200..300).contains(&status) {
        return Err(failed(status_detail(status, &body)));
    }
    let invalid = || FlowError::Message("Invalid device code response".into());
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|_| invalid())?;
    let object = value.as_object().ok_or_else(invalid)?;
    let string = |key: &str| object.get(key).and_then(serde_json::Value::as_str);
    let number = |key: &str| object.get(key).and_then(serde_json::Value::as_f64);
    match (
        string("device_code"),
        string("user_code"),
        string("verification_uri"),
        number("interval"),
        number("expires_in"),
    ) {
        (
            Some(device_code),
            Some(user_code),
            Some(verification_uri),
            Some(interval),
            Some(expires_in),
        ) => Ok(DeviceCode {
            device_code: device_code.to_owned(),
            user_code: user_code.to_owned(),
            verification_uri: verification_uri.to_owned(),
            interval,
            expires_in,
        }),
        _ => Err(FlowError::Message(
            "Invalid device code response fields".into(),
        )),
    }
}

enum Poll {
    Token(String),
    Pending,
    SlowDown(Option<f64>),
}

/// One token request; every failure is final.
async fn poll_once(
    ctx: &FlowCtx,
    host: &str,
    client_id: &str,
    device_code: &str,
) -> Result<Poll, FlowError> {
    let failed = |detail: String| {
        FlowError::Message(format!(
            "github-copilot device token request failed: {detail}"
        ))
    };
    let response = post(
        ctx,
        format!("https://{host}/login/oauth/access_token"),
        form(&[
            ("client_id", client_id),
            ("device_code", device_code),
            ("grant_type", GRANT_TYPE),
        ]),
    )
    .send()
    .await
    .map_err(|_| failed("transport unavailable".into()))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|_| failed("transport unavailable".into()))?;
    if !(200..300).contains(&status) {
        return Err(failed(status_detail(status, &body)));
    }
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        FlowError::Message("github-copilot device token response was not JSON".into())
    })?;
    let Some(object) = value.as_object() else {
        return Ok(Poll::Pending);
    };
    if let Some(token) = object
        .get("access_token")
        .and_then(serde_json::Value::as_str)
    {
        return Ok(Poll::Token(token.to_owned()));
    }
    match object.get("error").and_then(serde_json::Value::as_str) {
        None | Some("authorization_pending") => Ok(Poll::Pending),
        Some("slow_down") => Ok(Poll::SlowDown(
            object.get("interval").and_then(serde_json::Value::as_f64),
        )),
        Some(error) => Err(FlowError::Message(if is_safe_id(error) {
            format!("Device flow failed: {error}")
        } else {
            "Device flow failed".into()
        })),
    }
}

/// `^[a-z0-9_]{1,64}$`: the only error text echoed from the token endpoint.
fn is_safe_id(error: &str) -> bool {
    (1..=64).contains(&error.len())
        && error
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// omp's loop: sleep first (capped at the deadline), then poll; the deadline
/// is not rechecked between waking and posting.
async fn poll(
    ctx: &FlowCtx,
    host: &str,
    client_id: &str,
    device: &DeviceCode,
) -> Result<String, FlowError> {
    let deadline =
        (ctx.clock)().saturating_add(whole_ms(device.expires_in).min(i64::MAX as u64) as i64);
    let mut cadence = Cadence::new(device.interval);
    let mut slow_downs = 0u32;
    loop {
        let now = (ctx.clock)();
        if now >= deadline {
            break;
        }
        cancelled(ctx)?;
        let remaining = u64::try_from(deadline - now).unwrap_or(0);
        (ctx.sleep)(Duration::from_millis(cadence.wait_ms().min(remaining))).await;
        cancelled(ctx)?;
        match poll_once(ctx, host, client_id, &device.device_code).await? {
            Poll::Token(token) => return Ok(token),
            Poll::Pending => {}
            Poll::SlowDown(interval) => {
                slow_downs += 1;
                cadence.slow_down(interval);
            }
        }
    }
    Err(FlowError::Message(
        if slow_downs > 0 {
            SLOW_DOWN_TIMEOUT
        } else {
            TIMEOUT
        }
        .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_follows_omp_multipliers_in_whole_milliseconds() {
        let mut cadence = Cadence::new(5.0);
        assert_eq!(cadence.wait_ms(), 6000);
        cadence.slow_down(None);
        assert_eq!(cadence.wait_ms(), 14000);
        cadence.slow_down(Some(7.0));
        assert_eq!(cadence.wait_ms(), 9800);
        // A sub-second interval is floored to whole ms, then raised to 1 s.
        assert_eq!(Cadence::new(0.3).wait_ms(), 1200);
        // `interval: 0` on a slow_down adds 5 s.
        let mut zero = Cadence::new(5.0);
        zero.slow_down(Some(0.0));
        assert_eq!(zero.interval_ms, 10_000);
        // ceil rounding: 1001 ms × 1.2 = 1201.2 → 1202.
        assert_eq!(Cadence::new(1.0015).wait_ms(), 1202);
    }

    #[test]
    fn only_lowercase_protocol_ids_are_echoed() {
        assert!(is_safe_id("access_denied"));
        assert!(is_safe_id("expired_token"));
        assert!(!is_safe_id("Access Denied"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id(&"a".repeat(65)));
    }

    #[test]
    fn device_forms_keep_omp_field_order() {
        assert_eq!(
            form(&[("client_id", domain::PUBLIC_CLIENT_ID), ("scope", SCOPE)]),
            "client_id=Ov23li8tweQw6odWQebz&scope=read%3Auser"
        );
        assert_eq!(
            form(&[
                ("client_id", "c"),
                ("device_code", "d"),
                ("grant_type", GRANT_TYPE)
            ]),
            "client_id=c&device_code=d&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        );
    }
}
