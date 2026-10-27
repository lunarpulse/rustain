//! RFC 8628 device authorization grant, adapted from oh-my-pi's device-code login
//! engine and poll loop. Every wait goes through `FlowCtx::sleep`, every deadline is
//! computed from `FlowCtx::clock`, and cancellation is the `flows::login` select.
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
    token::{self, Grant, TokenFailure, TokenReply, Values},
};
use crate::domain::models::{
    Credential, OAuthCredential,
    auth_policy::{BodyEncoding, Rfc8628Policy},
};
use std::time::Duration;

const MINIMUM_INTERVAL_MS: f64 = 1000.0;
const DEFAULT_INTERVAL_SECONDS: f64 = 5.0;
const SLOW_DOWN_INCREMENT_MS: f64 = 5000.0;
const TIMEOUT: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";

struct DeviceAuthorization {
    user_code: String,
    device_code: String,
    verification_url: String,
    interval_seconds: Option<f64>,
    expires_in_seconds: Option<f64>,
}

enum PollOutcome {
    Complete(serde_json::Value),
    Pending,
    SlowDown,
    Failed(String),
}

pub(super) async fn run(
    provider_id: &str,
    policy: &'static Rfc8628Policy,
    ctx: &FlowCtx,
) -> Result<(Credential, bool), FlowError> {
    let client = token::client_id(policy.client_id)?;
    ctx.ui.on_progress("Requesting device authorization...");
    let device = authorize(provider_id, policy, &client, ctx).await?;
    let code = device.user_code.as_str();
    let instructions = policy.instructions.replace("{user_code}", code);
    // Displayed verbatim: the operator visits the provider's own page.
    ctx.ui
        .on_device_code(&device.verification_url, code, &instructions);
    ctx.ui.on_progress("Waiting for device authorization...");
    // Once per login, after the display and before the first poll (it may be discovered).
    let url = token::resolve_url(&ctx.http, policy.token.url)
        .await
        .map_err(FlowError::Message)?;
    let values = Values {
        client_id: &client,
        device_code: &device.device_code,
        ..Default::default()
    };
    let body = poll(provider_id, policy, &url, &values, &device, ctx).await?;
    let present = body
        .pointer(policy.projection.access)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|access| !access.trim().is_empty());
    if !present {
        return Err(FlowError::Message(format!(
            "{provider_id} token response missing access token"
        )));
    }
    let expires = body
        .pointer(policy.projection.expires_in)
        .and_then(serde_json::Value::as_f64)
        .is_some_and(f64::is_finite);
    if !expires {
        return Err(FlowError::Message(format!(
            "{provider_id} token response missing {}",
            policy
                .projection
                .expires_in
                .trim_start_matches('/')
                .replace('/', ".")
        )));
    }
    let now = (ctx.clock)();
    let mut projected = token::project(&body, policy.projection, now)?;
    super::identity::fill(
        &ctx.http,
        policy.identity_hook,
        &projected.access,
        &body,
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
        }),
        true,
    ))
}

async fn authorize(
    provider_id: &str,
    policy: &Rfc8628Policy,
    client: &str,
    ctx: &FlowCtx,
) -> Result<DeviceAuthorization, FlowError> {
    let failed = |detail: String| {
        FlowError::Message(format!(
            "{provider_id} device authorization failed: {detail}"
        ))
    };
    let values = Values {
        client_id: client,
        scope: policy.scope,
        ..Default::default()
    };
    let mut fields = vec![("client_id", client)];
    if !policy.scope.is_empty() {
        fields.push(("scope", policy.scope));
    }
    let mut request = ctx
        .http
        .post(crate::adapters::auth::policies::endpoint(policy.device_url))
        .timeout(Duration::from_millis(policy.timeout_ms));
    for (name, tpl) in policy.device_headers {
        request = request.header(*name, values.render(*tpl));
    }
    request = match policy.encoding {
        BodyEncoding::Json => request
            .header("Content-Type", "application/json")
            .body(json_object(&fields)),
        BodyEncoding::Form => request
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(
                url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(fields)
                    .finish(),
            ),
    };
    let response = request
        .send()
        .await
        .map_err(|_| failed("transport unavailable".to_owned()))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let status = status.as_u16();
        // Only a whitelisted protocol id may follow the status; bodies are never echoed.
        let id = TokenFailure {
            status: Some(status),
            body,
        }
        .safe_id();
        return Err(failed(match id {
            Some(id) => format!("{status} {id}"),
            None => status.to_string(),
        }));
    }
    let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let string = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let number = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_f64)
            .filter(|v| v.is_finite())
    };
    let (Some(user_code), Some(device_code), Some(verification_uri)) = (
        string("user_code"),
        string("device_code"),
        string("verification_uri"),
    ) else {
        return Err(FlowError::Message(format!(
            "{provider_id} device authorization response missing required fields"
        )));
    };
    Ok(DeviceAuthorization {
        user_code,
        device_code,
        verification_url: string("verification_uri_complete").unwrap_or(verification_uri),
        interval_seconds: number("interval"),
        expires_in_seconds: number("expires_in"),
    })
}

/// Polls immediately, then sleeps `min(interval, remaining)` after each pending or
/// `slow_down` reply. A complete reply wins even at the deadline; any other reply,
/// or a poll error, at or after the deadline becomes the timeout text.
async fn poll(
    provider_id: &str,
    policy: &Rfc8628Policy,
    url: &str,
    values: &Values<'_>,
    device: &DeviceAuthorization,
    ctx: &FlowCtx,
) -> Result<serde_json::Value, FlowError> {
    let now = || (ctx.clock)() as f64;
    let deadline = device
        .expires_in_seconds
        .map(|seconds| now() + seconds * 1000.0);
    let past = || deadline.is_some_and(|deadline| now() >= deadline);
    let mut interval_ms = initial_interval_ms(device.interval_seconds);
    let mut slow_downs = 0_u32;
    let cancel = ctx.ui.cancel_token();
    loop {
        if past() {
            break;
        }
        // omp checks the caller's signal before every poll: a sleeper may return
        // while cancellation is pending, and no poll may follow it.
        if cancel.is_cancelled() {
            return Err(FlowError::Cancelled);
        }
        let request = token::request(&ctx.http, policy.token, url, Grant::DeviceCode, values);
        let reply = match deadline {
            // The device code's expiry also aborts an in-flight poll, which is a timeout.
            Some(deadline) => match tokio::time::timeout(millis(deadline - now()), request).await {
                Ok(reply) => reply,
                Err(_) => break,
            },
            None => request.await,
        };
        let outcome = match reply {
            Ok(reply) => classify(provider_id, reply),
            Err(_) if past() => break,
            Err(_) => {
                return Err(FlowError::Message(format!(
                    "{provider_id} device token request failed: transport unavailable"
                )));
            }
        };
        let outcome = match outcome {
            PollOutcome::Complete(body) => return Ok(body),
            _ if past() => break,
            outcome => outcome,
        };
        match outcome {
            PollOutcome::Failed(message) => return Err(FlowError::Message(message)),
            PollOutcome::SlowDown => {
                slow_downs += 1;
                interval_ms += SLOW_DOWN_INCREMENT_MS;
            }
            PollOutcome::Pending | PollOutcome::Complete(_) => {}
        }
        let remaining = deadline.map_or(f64::INFINITY, |deadline| deadline - now());
        if remaining <= 0.0 {
            break;
        }
        (ctx.sleep)(millis(interval_ms.min(remaining))).await;
    }
    let message = if slow_downs > 0 {
        SLOW_DOWN_TIMEOUT
    } else {
        TIMEOUT
    };
    Err(FlowError::Message(message.to_owned()))
}

fn classify(provider_id: &str, reply: TokenReply) -> PollOutcome {
    // JavaScript `error === undefined`: a present `null` is still an error reply.
    let error = reply.body.get("error");
    if (200..300).contains(&reply.status) && error.is_none() {
        return PollOutcome::Complete(reply.body);
    }
    match error.and_then(serde_json::Value::as_str) {
        Some("authorization_pending") => PollOutcome::Pending,
        Some("slow_down") => PollOutcome::SlowDown,
        Some("expired_token") => PollOutcome::Failed(format!(
            "{provider_id} device code expired; restart the login"
        )),
        Some("access_denied") => {
            PollOutcome::Failed(format!("{provider_id} device authorization was denied"))
        }
        _ => {
            let status = reply.status;
            let id = TokenFailure {
                status: Some(status),
                body: reply.body.to_string(),
            }
            .safe_id();
            PollOutcome::Failed(match id {
                Some(id) => format!("{provider_id} device token request failed: {status} {id}"),
                None => format!("{provider_id} device token request failed: {status}"),
            })
        }
    }
}

/// RFC 8628 default of 5 s, floored to whole milliseconds, never below 1 s.
fn initial_interval_ms(seconds: Option<f64>) -> f64 {
    (seconds.unwrap_or(DEFAULT_INTERVAL_SECONDS) * 1000.0)
        .floor()
        .max(MINIMUM_INTERVAL_MS)
}

/// Whole milliseconds, rounded up so a wait never ends before the deadline it targets.
fn millis(ms: f64) -> Duration {
    Duration::from_millis(ms.max(0.0).ceil() as u64)
}

/// A JSON object in field order; omp sends standard params as written.
fn json_object(fields: &[(&str, &str)]) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::Value::from(*key).to_string());
        out.push(':');
        out.push_str(&serde_json::Value::from(*value).to_string());
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interval_follows_rfc_default_and_minimum() {
        assert_eq!(initial_interval_ms(None), 5000.0);
        assert_eq!(initial_interval_ms(Some(5.0)), 5000.0);
        assert_eq!(initial_interval_ms(Some(2.5)), 2500.0);
        assert_eq!(initial_interval_ms(Some(1.0009)), 1000.0);
        assert_eq!(initial_interval_ms(Some(0.5)), 1000.0);
        assert_eq!(initial_interval_ms(Some(0.0)), 1000.0);
        assert_eq!(initial_interval_ms(Some(-3.0)), 1000.0);
    }

    #[test]
    fn millis_rounds_up_and_clamps_negative() {
        assert_eq!(millis(5000.0), Duration::from_millis(5000));
        assert_eq!(millis(1999.2), Duration::from_millis(2000));
        assert_eq!(millis(-1.0), Duration::ZERO);
    }

    fn reply(status: u16, body: serde_json::Value) -> TokenReply {
        TokenReply { status, body }
    }

    fn failed(outcome: PollOutcome) -> String {
        match outcome {
            PollOutcome::Failed(message) => message,
            _ => panic!("expected a failure"),
        }
    }

    #[test]
    fn classification_texts_never_echo_bodies() {
        assert!(matches!(
            classify("xai-oauth", reply(200, json!({"access_token": "a"}))),
            PollOutcome::Complete(_)
        ));
        assert!(matches!(
            classify("xai-oauth", reply(200, serde_json::Value::Null)),
            PollOutcome::Complete(_)
        ));
        assert!(matches!(
            classify(
                "xai-oauth",
                reply(400, json!({"error": "authorization_pending"}))
            ),
            PollOutcome::Pending
        ));
        assert!(matches!(
            classify("xai-oauth", reply(200, json!({"error": "slow_down"}))),
            PollOutcome::SlowDown
        ));
        assert_eq!(
            failed(classify(
                "xai-oauth",
                reply(400, json!({"error": "expired_token"}))
            )),
            "xai-oauth device code expired; restart the login"
        );
        assert_eq!(
            failed(classify(
                "xai-oauth",
                reply(400, json!({"error": "access_denied"}))
            )),
            "xai-oauth device authorization was denied"
        );
        assert_eq!(
            failed(classify(
                "xai-oauth",
                reply(
                    400,
                    json!({"error": "invalid_scope", "error_description": "SECRET-DETAIL"})
                )
            )),
            "xai-oauth device token request failed: 400"
        );
        assert_eq!(
            failed(classify(
                "xai-oauth",
                reply(400, json!({"error": "invalid_grant"}))
            )),
            "xai-oauth device token request failed: 400 invalid_grant"
        );
        assert_eq!(
            failed(classify("xai-oauth", reply(500, serde_json::Value::Null))),
            "xai-oauth device token request failed: 500"
        );
        assert_eq!(
            failed(classify("xai-oauth", reply(200, json!({"error": null})))),
            "xai-oauth device token request failed: 200"
        );
    }

    #[test]
    fn json_device_body_keeps_field_order() {
        assert_eq!(
            json_object(&[("client_id", "c\"1"), ("scope", "a b")]),
            r#"{"client_id":"c\"1","scope":"a b"}"#
        );
    }
}
