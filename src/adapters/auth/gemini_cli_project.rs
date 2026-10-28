//! Cloud Code Assist project discovery and onboarding after a Gemini CLI login,
//! adapted from oh-my-pi (ai/src/registry/oauth/google-gemini-cli.ts,
//! google-oauth-shared.ts, ai/src/utils/google-validation.ts,
//! catalog/src/wire/gemini-headers.ts).
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

use crate::adapters::auth::flows::{FlowCtx, FlowError};
use crate::domain::models::{CredentialIdentity, SecretString};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use std::time::Duration;

const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
/// Every provisioning request (omp `OAUTH_REQUEST_TIMEOUT_MS`).
const REQUEST_TIMEOUT_MS: u64 = 30_000;
const POLL_INTERVAL: Duration = Duration::from_millis(5_000);
/// omp `POLL_MAX_ATTEMPTS`: 24 GETs, at most 23 sleeps.
pub const POLL_MAX_ATTEMPTS: u32 = 24;
const TIER_FREE: &str = "free-tier";
const TIER_LEGACY: &str = "legacy-tier";
const TIER_STANDARD: &str = "standard-tier";
const PROJECT_REQUIRED: &str = "This account requires setting the GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID environment variable. See https://goo.gle/gemini-cli-auth-docs#workspace-gca";
const NO_PROJECT: &str = "Could not discover or provision a Google Cloud project. Try setting the GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID environment variable. See https://goo.gle/gemini-cli-auth-docs#workspace-gca";
/// `getGeminiCliHeaders()` without a model names this one.
pub const DEFAULT_HEADER_MODEL: &str = "gemini-3.1-pro-preview";
/// Pinned wire identity; there is no runtime override.
const GEMINI_CLI_VERSION: &str = "0.46.0";
pub const CLIENT_METADATA: &str =
    "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";

/// `GeminiCLI/<version>/<model> (<platform>; <arch>; terminal)` with Node platform names.
pub fn user_agent(model: &str) -> String {
    format!(
        "GeminiCLI/{GEMINI_CLI_VERSION}/{model} ({}; {}; terminal)",
        node_platform(std::env::consts::OS),
        node_arch(std::env::consts::ARCH)
    )
}

/// Rust `std::env::consts::OS` → Node `process.platform`.
pub fn node_platform(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Rust `std::env::consts::ARCH` → Node `process.arch`.
pub fn node_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// omp `extractGoogleValidationUrl`, tightened: only an absolute `https:` URL
/// without control characters is shown to the operator.
pub fn validation_url(body: &str) -> Option<String> {
    if !body.contains("VALIDATION_REQUIRED") {
        return None;
    }
    let start = body.find('{')?;
    let parsed: Value = serde_json::from_str(&body[start..]).ok()?;
    let url = parsed
        .pointer("/error/details")?
        .as_array()?
        .iter()
        .find(|detail| {
            detail["reason"] == "VALIDATION_REQUIRED"
                && detail["metadata"]["validation_url"].is_string()
        })?["metadata"]["validation_url"]
        .as_str()?;
    let safe = !url.chars().any(char::is_control)
        && url::Url::parse(url).is_ok_and(|parsed| parsed.scheme() == "https");
    safe.then(|| url.to_owned())
}

/// omp `formatGoogleValidationRequiredMessage`.
pub fn validation_message(url: &str, next_action: &str, email: Option<&str>) -> String {
    let account = email
        .map(|email| format!(" for {email}"))
        .unwrap_or_default();
    format!("Account verification required{account}. Visit {url} to continue, then {next_action}.")
}

/// The after-exchange hook: requires a refresh token, then discovers or provisions
/// the project and sets `identity.project_id`. Every failure stores nothing.
pub(crate) async fn run(
    http: &reqwest::Client,
    ctx: &FlowCtx,
    raw: &Value,
    access: &SecretString,
    identity: &mut CredentialIdentity,
) -> Result<(), FlowError> {
    if raw
        .get("refresh_token")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(FlowError::Message(
            "No refresh token received. Please try again.".into(),
        ));
    }
    let session = Session {
        http,
        ctx,
        headers: headers(access)?,
    };
    match session.discover().await {
        Ok(project) => {
            identity.project_id = Some(project);
            Ok(())
        }
        Err(Failure::Validation(url)) => Err(FlowError::Message(validation_message(
            &url,
            "sign in again",
            identity.email.as_deref(),
        ))),
        Err(Failure::Message(message)) => Err(FlowError::Message(message)),
    }
}

/// One consolidated exposure: the bearer header of every provisioning request,
/// marked sensitive. Redaction reads it back from the header map.
fn headers(access: &SecretString) -> Result<HeaderMap, FlowError> {
    let mut bearer =
        HeaderValue::try_from(format!("Bearer {}", access.expose_secret())).map_err(|_| {
            FlowError::Message("OAuth token response has an invalid access_token".into())
        })?;
    bearer.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert("authorization", bearer);
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    if let Ok(agent) = HeaderValue::try_from(user_agent(DEFAULT_HEADER_MODEL)) {
        headers.insert("user-agent", agent);
    }
    headers.insert("client-metadata", HeaderValue::from_static(CLIENT_METADATA));
    Ok(headers)
}

enum Failure {
    /// A `VALIDATION_REQUIRED` failure body carrying a showable URL.
    Validation(String),
    Message(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

struct Session<'a> {
    http: &'a reqwest::Client,
    ctx: &'a FlowCtx,
    headers: HeaderMap,
}

struct Reply {
    status: reqwest::StatusCode,
    body: String,
}

impl Session<'_> {
    async fn send(&self, request: reqwest::RequestBuilder, url: &str) -> Result<Reply, Failure> {
        let sent = async {
            let response = request.headers(self.headers.clone()).send().await?;
            let status = response.status();
            let body = response.text().await?;
            Ok::<_, reqwest::Error>(Reply { status, body })
        };
        match tokio::time::timeout(Duration::from_millis(REQUEST_TIMEOUT_MS), sent).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) if !error.is_timeout() => Err(Failure::Message(format!(
                "Could not reach {url}: {}",
                crate::adapters::provider::classify_reqwest_error(&error)
            ))),
            _ => Err(Failure::Message(format!(
                "Timed out after {REQUEST_TIMEOUT_MS}ms waiting for {url}"
            ))),
        }
    }

    async fn post(&self, path: &str, body: String) -> Result<Reply, Failure> {
        let url = crate::adapters::auth::policies::endpoint(&format!(
            "{CODE_ASSIST_ENDPOINT}/v1internal:{path}"
        ));
        self.send(self.http.post(&url).body(body), &url).await
    }

    /// `<status> <reason>: <message>`: the parsed `error.message`, else the body cut
    /// to 500 characters, with the bearer removed before the cut.
    fn failure(&self, step: &str, reply: &Reply) -> Failure {
        if let Some(url) = validation_url(&reply.body) {
            return Failure::Validation(url);
        }
        let value: Value = serde_json::from_str(&reply.body).unwrap_or(Value::Null);
        let message = match value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
        {
            Some(message) => self.redact(message),
            None => self.redact(&reply.body).chars().take(500).collect(),
        };
        Failure::Message(format!(
            "{step} failed: {} {}: {message}",
            reply.status.as_u16(),
            reply.status.canonical_reason().unwrap_or("")
        ))
    }

    fn redact(&self, text: &str) -> String {
        let token = self
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|token| !token.is_empty());
        match token {
            Some(token) => text.replace(token, "[redacted]"),
            None => text.to_owned(),
        }
    }

    fn json(step: &str, reply: &Reply) -> Result<Value, Failure> {
        serde_json::from_str(&reply.body)
            .map_err(|_| Failure::Message(format!("{step} returned invalid JSON")))
    }

    async fn discover(&self) -> Result<String, Failure> {
        let env_project = crate::infrastructure::utils::env_var_trimmed("GOOGLE_CLOUD_PROJECT")
            .or_else(|| crate::infrastructure::utils::env_var_trimmed("GOOGLE_CLOUD_PROJECT_ID"));
        self.ctx
            .ui
            .on_progress("Checking for existing Cloud Code Assist project...");
        let reply = self
            .post("loadCodeAssist", load_body(env_project.as_deref()))
            .await?;
        let data = if reply.status.is_success() {
            Self::json("loadCodeAssist", &reply)?
        } else if vpc_sc_affected(&reply.body) {
            serde_json::json!({"currentTier": {"id": TIER_STANDARD}})
        } else {
            return Err(self.failure("loadCodeAssist", &reply));
        };
        if data.get("currentTier").is_some_and(|tier| !tier.is_null()) {
            return project_of(data.get("cloudaicompanionProject"))
                .or(env_project)
                .ok_or_else(|| Failure::Message(PROJECT_REQUIRED.into()));
        }
        let tier_id = default_tier(data.get("allowedTiers"));
        if tier_id != TIER_FREE && env_project.is_none() {
            return Err(Failure::Message(PROJECT_REQUIRED.into()));
        }
        self.ctx
            .ui
            .on_progress("Provisioning Cloud Code Assist project (this may take a moment)...");
        let paid_project = env_project.as_deref().filter(|_| tier_id != TIER_FREE);
        let reply = self
            .post("onboardUser", onboard_body(&tier_id, paid_project))
            .await?;
        if !reply.status.is_success() {
            return Err(self.failure("onboardUser", &reply));
        }
        let mut operation = Self::json("onboardUser", &reply)?;
        if !truthy(operation.get("done"))
            && let Some(name) = operation
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
        {
            operation = self.poll(name).await?;
        }
        operation
            .pointer("/response/cloudaicompanionProject/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .or(env_project)
            .ok_or_else(|| Failure::Message(NO_PROJECT.into()))
    }

    /// omp `pollOperation`: attempt 0 is immediate; each later attempt reports,
    /// waits 5 s through the injected sleeper, then polls.
    async fn poll(&self, name: &str) -> Result<Value, Failure> {
        let url = crate::adapters::auth::policies::endpoint(&format!(
            "{CODE_ASSIST_ENDPOINT}/v1internal/{name}"
        ));
        for attempt in 0..POLL_MAX_ATTEMPTS {
            if attempt > 0 {
                self.ctx.ui.on_progress(&format!(
                    "Waiting for project provisioning (attempt {}/{POLL_MAX_ATTEMPTS})...",
                    attempt + 1
                ));
                (self.ctx.sleep)(POLL_INTERVAL).await;
            }
            let reply = self.send(self.http.get(&url), &url).await?;
            if !reply.status.is_success() {
                return Err(Failure::Message(format!(
                    "Failed to poll operation: {} {}",
                    reply.status.as_u16(),
                    reply.status.canonical_reason().unwrap_or("")
                )));
            }
            let data = Self::json("Operation poll", &reply)?;
            if truthy(data.get("done")) {
                return Ok(data);
            }
        }
        Err(Failure::Message(format!(
            "Project provisioning did not complete after {POLL_MAX_ATTEMPTS} attempts"
        )))
    }
}

/// JavaScript truthiness of an optional JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// A non-empty string or `{id: string}` (the shape omp's discovery also accepts).
fn project_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(id) => Some(id.clone()),
        Value::Object(object) => object.get("id").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
    .filter(|id| !id.is_empty())
}

/// omp `getDefaultTier(...).id ?? "free-tier"`.
fn default_tier(allowed: Option<&Value>) -> String {
    let tiers = allowed
        .and_then(Value::as_array)
        .filter(|tiers| !tiers.is_empty());
    let Some(tiers) = tiers else {
        return TIER_LEGACY.into();
    };
    match tiers.iter().find(|tier| truthy(tier.get("isDefault"))) {
        Some(tier) => tier
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(TIER_FREE)
            .to_owned(),
        None => TIER_LEGACY.into(),
    }
}

fn vpc_sc_affected(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.pointer("/error/details")?.as_array().cloned())
        .is_some_and(|details| {
            details
                .iter()
                .any(|detail| detail["reason"] == "SECURITY_POLICY_VIOLATED")
        })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Metadata<'a> {
    ide_type: &'static str,
    platform: &'static str,
    plugin_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    duet_project: Option<&'a str>,
}

fn metadata(project: Option<&str>) -> Metadata<'_> {
    Metadata {
        ide_type: "IDE_UNSPECIFIED",
        platform: "PLATFORM_UNSPECIFIED",
        plugin_type: "GEMINI",
        duet_project: project,
    }
}

/// `JSON.stringify` order; undefined fields are dropped.
fn load_body(project: Option<&str>) -> String {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Load<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        cloudaicompanion_project: Option<&'a str>,
        metadata: Metadata<'a>,
    }
    serde_json::to_string(&Load {
        cloudaicompanion_project: project,
        metadata: metadata(project),
    })
    .unwrap_or_default()
}

/// Insertion order: `tierId`, `metadata`, then the paid tier's project.
fn onboard_body(tier_id: &str, project: Option<&str>) -> String {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Onboard<'a> {
        tier_id: &'a str,
        metadata: Metadata<'a>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cloudaicompanion_project: Option<&'a str>,
    }
    serde_json::to_string(&Onboard {
        tier_id,
        metadata: metadata(project),
        cloudaicompanion_project: project,
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_and_arch_use_node_names() {
        assert_eq!(node_platform("macos"), "darwin");
        assert_eq!(node_platform("windows"), "win32");
        assert_eq!(node_platform("linux"), "linux");
        assert_eq!(node_arch("x86_64"), "x64");
        assert_eq!(node_arch("aarch64"), "arm64");
        assert_eq!(node_arch("riscv64"), "riscv64");
        assert!(user_agent("gemini-2.5-pro").starts_with("GeminiCLI/0.46.0/gemini-2.5-pro ("));
        assert!(user_agent("m").ends_with("; terminal)"));
    }

    #[test]
    fn bodies_follow_json_stringify_order() {
        assert_eq!(
            load_body(None),
            r#"{"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}"#
        );
        assert_eq!(
            load_body(Some("P")),
            r#"{"cloudaicompanionProject":"P","metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI","duetProject":"P"}}"#
        );
        assert_eq!(
            onboard_body("free-tier", None),
            r#"{"tierId":"free-tier","metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}"#
        );
        assert_eq!(
            onboard_body("standard-tier", Some("P")),
            r#"{"tierId":"standard-tier","metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI","duetProject":"P"},"cloudaicompanionProject":"P"}"#
        );
    }

    #[test]
    fn tier_choice_and_project_forms() {
        assert_eq!(default_tier(None), TIER_LEGACY);
        assert_eq!(default_tier(Some(&serde_json::json!([]))), TIER_LEGACY);
        assert_eq!(
            default_tier(Some(&serde_json::json!([{"id": "x"}]))),
            TIER_LEGACY
        );
        assert_eq!(
            default_tier(Some(&serde_json::json!([{"id": "x"}, {"isDefault": true}]))),
            TIER_FREE
        );
        assert_eq!(
            default_tier(Some(
                &serde_json::json!([{"id": "standard-tier", "isDefault": true}])
            )),
            "standard-tier"
        );
        assert_eq!(
            project_of(Some(&serde_json::json!("p"))).as_deref(),
            Some("p")
        );
        assert_eq!(
            project_of(Some(&serde_json::json!({"id": "q"}))).as_deref(),
            Some("q")
        );
        assert_eq!(project_of(Some(&serde_json::json!(""))), None);
        assert_eq!(project_of(Some(&serde_json::json!({"id": 3}))), None);
    }

    #[test]
    fn validation_url_is_https_only() {
        let body = |url: &str| {
            format!(
                r#"HTTP 403 {{"error":{{"details":[{{"reason":"VALIDATION_REQUIRED","metadata":{{"validation_url":"{url}"}}}}]}}}}"#
            )
        };
        assert_eq!(
            validation_url(&body("https://accounts.google.com/v?x=1")).as_deref(),
            Some("https://accounts.google.com/v?x=1")
        );
        assert_eq!(validation_url(&body("http://evil.example/v")), None);
        assert_eq!(validation_url(&body("javascript:alert(1)")), None);
        assert_eq!(validation_url(&body("https://a.example/\\u0007")), None);
        assert_eq!(validation_url(r#"{"error":{}}"#), None);
        assert_eq!(
            validation_message("https://v", "sign in again", Some("a@b")),
            "Account verification required for a@b. Visit https://v to continue, then sign in again."
        );
        assert_eq!(
            validation_message("https://v", "retry your request", None),
            "Account verification required. Visit https://v to continue, then retry your request."
        );
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use std::sync::Arc;

    struct Silent;
    impl crate::adapters::auth::flows::LoginUi for Silent {
        fn on_auth_url(&self, _: &str, _: &str, _: &str) {}
        fn on_device_code(&self, _: &str, _: &str, _: &str) {}
        fn on_progress(&self, _: &str) {}
        fn prompt_secret(&self, _: &str) -> futures::future::BoxFuture<'_, Option<SecretString>> {
            Box::pin(async { None })
        }
        fn manual_input(
            &self,
            _: &str,
        ) -> Option<futures::future::BoxFuture<'static, Option<String>>> {
            None
        }
        fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
            tokio_util::sync::CancellationToken::new()
        }
    }

    /// Paused time against a local endpoint that accepts and never answers.
    #[tokio::test(start_paused = true)]
    async fn each_provisioning_request_is_bounded_to_30_seconds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1internal:loadCodeAssist",
            listener.local_addr().unwrap()
        );
        let hold = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
            drop(socket);
        });
        let http = reqwest::Client::new();
        let ctx = FlowCtx::new(Arc::new(Silent), Arc::new(|| 0));
        let session = Session {
            http: &http,
            ctx: &ctx,
            headers: HeaderMap::new(),
        };
        let start = tokio::time::Instant::now();
        let Err(Failure::Message(message)) = session.send(http.post(&url), &url).await else {
            panic!("a held request must time out");
        };
        assert_eq!(
            message,
            format!("Timed out after 30000ms waiting for {url}")
        );
        assert_eq!(start.elapsed(), Duration::from_millis(REQUEST_TIMEOUT_MS));
        hold.abort();
    }
}
