//! Copilot request identity: API headers, per-request dynamic headers, the
//! effective API root of a credential and the error mapping, adapted from
//! oh-my-pi (ai/src/providers/github-copilot-headers.ts,
//! catalog/src/wire/github-copilot.ts, ai/src/utils/http-inspector.ts).
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

use std::{borrow::Cow, sync::Arc, sync::LazyLock};

use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::catalog::{self, Route};
use super::domain::{enterprise_domain, enterprise_root, normalize_api_endpoint, parse_api_key};
use super::identity::{self, CopilotRequestState};
use super::{DEFAULT_BASE_URL, Shared};
use crate::adapters::openai::profile::{ChatCompat, ChatImages, ChatProfile};
use crate::adapters::provider::{
    http_retry::{HttpRetryPolicy, OPENAI_WIRE},
    request_profile::{RequestContext, RequestProfile, RequestState},
    stream_retry::{CHAT, MESSAGES, StreamResilience},
};
use crate::domain::{
    errors::ProviderError,
    models::{Message, MessageRole, ModelCapability, ResolvedAuth, SecretString},
};
use crate::infrastructure::key_resolver::Resolved;

/// Copilot CLI wire identity (`copilot/<version>`).
pub const USER_AGENT: &str = "copilot/1.0.82";
const HARNESS_ID: &str = "copilot-sdk";
const INTENT: &str = "conversation-agent";
/// Sent to Copilot API hosts only, never to `api.github.com`.
const API_VERSION: &str = "2026-08-01";
/// The first-event timeout in seconds, announced as `X-Stainless-Timeout`.
const STAINLESS_TIMEOUT: &str = "300";
/// omp `http-inspector.ts` 403 text, with the product name rustain.
pub const FORBIDDEN: &str = "GitHub Copilot access denied (HTTP 403). Your token is valid but the account may not have access to this model or feature. Check your Copilot plan or model policy settings. Business organizations can also restrict which clients may call the API: rustain sends Copilot-Integration-Id copilot-chat by default (COPILOT_INTEGRATION_ID overrides it) and retries a denied default-identity request once as the Copilot CLI (copilot-developer-cli). If both identities are denied, ask your org admin to allow one of them; if you pinned an identity, try the other.";

fn put(out: &mut HeaderMap, name: &'static str, value: &'static str) {
    out.insert(
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    );
}

/// The six API headers of every Copilot API request (the CLI identity).
pub fn api_headers(out: &mut HeaderMap) {
    put(out, "user-agent", USER_AGENT);
    put(out, "editor-version", USER_AGENT);
    put(out, "copilot-integration-id", identity::CLI_INTEGRATION_ID);
    put(out, "copilot-harness-id", HARNESS_ID);
    put(out, "openai-intent", INTENT);
    put(out, "x-github-api-version", API_VERSION);
}

/// Model listing: the API headers plus `X-Initiator: user`.
pub fn discovery_headers(out: &mut HeaderMap) {
    api_headers(out);
    put(out, "x-initiator", "user");
}

/// The GitHub token of a resolved credential with its routing. A stored row
/// carries routing; an env value may be a JSON envelope (`token`,
/// `enterpriseUrl`, `apiEndpoint`), whose token alone is ever sent.
pub struct CopilotGrant<'a> {
    pub token: Cow<'a, str>,
    pub enterprise: Option<String>,
    pub endpoint: Option<String>,
}

/// Consolidated secret exposure: the GitHub token of a Copilot credential is
/// read here once per use, for the bearer header, the envelope parse, the
/// identity-cache digest and the routing probe.
fn expose(secret: &SecretString) -> &str {
    secret.expose_secret()
}

impl<'a> CopilotGrant<'a> {
    pub fn of(resolved: &'a Resolved) -> Self {
        Self::from_auth(&resolved.auth)
    }

    pub fn from_auth(auth: &'a ResolvedAuth) -> Self {
        match auth {
            ResolvedAuth::OAuth {
                access, routing, ..
            } => Self {
                token: Cow::Borrowed(expose(access)),
                enterprise: enterprise_domain(routing.enterprise_domain.as_deref()),
                endpoint: routing
                    .api_endpoint
                    .as_deref()
                    .and_then(normalize_api_endpoint),
            },
            other => {
                let envelope = parse_api_key(expose(other.bearer_secret()));
                Self {
                    token: envelope.token,
                    enterprise: envelope.enterprise,
                    endpoint: envelope.endpoint,
                }
            }
        }
    }

    /// The `Authorization` value of a Copilot API request: the token alone,
    /// never an env envelope.
    pub fn bearer(&self) -> Result<HeaderValue, ProviderError> {
        if self.token.trim().is_empty() {
            return Err(ProviderError::AuthenticationFailed);
        }
        let mut value = HeaderValue::try_from(format!("Bearer {}", self.token))
            .map_err(|_| ProviderError::Other("Invalid GitHub Copilot credential".into()))?;
        value.set_sensitive(true);
        Ok(value)
    }
}

/// An `https` URL on `githubcopilot.com` or one of its subdomains (a rustain
/// tightening of omp's substring test).
fn managed(base: &str) -> bool {
    url::Url::parse(base).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| {
                host == "githubcopilot.com" || host.ends_with(".githubcopilot.com")
            })
    })
}

/// A derived root is rebased in instrumented builds; a configured one never is.
pub(super) fn derived(root: &str) -> String {
    crate::adapters::auth::policies::endpoint(root)
        .trim_end_matches('/')
        .to_owned()
}

/// omp `resolveGitHubCopilotBaseUrl` over `learned.or(configured)`: the grant's
/// endpoint while the base is absent or managed; without an Enterprise domain
/// the base (or the canonical root); a foreign base survives an Enterprise
/// credential; else the Enterprise root.
pub fn copilot_root(
    configured: Option<&str>,
    grant: &CopilotGrant<'_>,
    learned: Option<&str>,
) -> String {
    // A learned root is derived (rebased); a configured base is sent verbatim.
    let base = learned
        .map(|root| (root, true))
        .or(configured.map(|base| (base, false)));
    let is_managed = base.is_none_or(|(base, _)| managed(base));
    if is_managed && let Some(endpoint) = &grant.endpoint {
        return derived(endpoint);
    }
    let keep = |(base, learned): (&str, bool)| {
        if learned {
            derived(base)
        } else {
            base.trim_end_matches('/').to_owned()
        }
    };
    match (base, &grant.enterprise) {
        (None, None) => derived(DEFAULT_BASE_URL),
        (Some(base), None) => keep(base),
        (Some(base), Some(_)) if !is_managed => keep(base),
        (_, Some(enterprise)) => derived(&enterprise_root(Some(enterprise))),
    }
}

/// omp `inferCopilotInitiator` over rustain's last message.
pub fn initiator(messages: &[Message]) -> &'static str {
    match messages.last() {
        None => "user",
        Some(message) => match message.role {
            MessageRole::Assistant | MessageRole::System => "agent",
            MessageRole::User if !message.tool_results.is_empty() => "agent",
            MessageRole::User => "user",
        },
    }
}

/// omp `hasCopilotVisionInput`: an image on a user message or a tool result.
pub fn vision(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message.role == MessageRole::User
            && (!message.images.is_empty()
                || message.tool_results.iter().any(|r| !r.images.is_empty()))
    })
}

/// The per-request dynamic headers except the integration id (request state).
pub fn dynamic_headers(messages: &[Message], out: &mut HeaderMap) {
    let initiator = initiator(messages);
    put(out, "x-initiator", initiator);
    put(
        out,
        "x-interaction-type",
        if initiator == "agent" {
            "conversation-agent"
        } else {
            "conversation-user"
        },
    );
    if vision(messages) {
        put(out, "copilot-vision-request", "true");
    }
}

/// Removes bearers and GitHub tokens from provider text, then the sent bearer.
/// A `token` is redacted only when a credential-length value follows it, so
/// prose such as `prompt token count of …` stays verbatim (omp keeps a 400's
/// text as is).
pub fn redact(message: &str, sent: &HeaderMap) -> String {
    static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]+|\btoken\s+[A-Za-z0-9._~+/=-]{20,}|\b(?:gh[opsu]_[A-Za-z0-9]{8,}|github_pat_[A-Za-z0-9_]{8,})")
            .expect("fixed token pattern")
    });
    let mut text = TOKENS.replace_all(message, "[redacted]").into_owned();
    if let Some(bearer) = sent
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty())
    {
        text = text.replace(bearer, "[redacted]");
    }
    text
}

/// The Copilot mapping of a final non-2xx response. A 403 is a plan, policy or
/// organization denial: never an authentication failure, never recovered.
pub fn http_error(status: u16, headers: &HeaderMap, body: &str, sent: &HeaderMap) -> ProviderError {
    match status {
        401 => return ProviderError::AuthenticationFailed,
        403 => return ProviderError::Other(FORBIDDEN.into()),
        429 => {
            return ProviderError::RateLimited {
                retry_after_ms: crate::adapters::responses::retry::hint(
                    headers,
                    body,
                    crate::infrastructure::utils::auth_now_ms(),
                ),
            };
        }
        _ => {}
    }
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let error = &value["error"];
    let message = match error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(message) => redact(message, sent),
        // Redact before the cut: no prefix of a bearer straddling it may survive.
        None => redact(body, sent).chars().take(500).collect(),
    };
    if status >= 500 {
        ProviderError::ConnectionFailed(format!("GitHub Copilot server error {status}: {message}"))
    } else {
        ProviderError::Other(format!("GitHub Copilot HTTP {status}: {message}"))
    }
}

/// The Copilot request policy of one route; every route shares the adapter's
/// `Shared` state (configured base, learned root, served catalogue).
#[derive(Debug)]
pub struct CopilotProfile {
    route: Route,
    shared: Arc<Shared>,
}

impl CopilotProfile {
    pub(crate) fn new(route: Route, shared: Arc<Shared>) -> Self {
        Self { route, shared }
    }

    /// The root this credential's requests are sent to.
    pub fn root(&self, resolved: &Resolved) -> String {
        let grant = CopilotGrant::of(resolved);
        let learned = self.shared.learned_root(resolved);
        copilot_root(
            self.shared.configured_base.as_deref(),
            &grant,
            learned.as_deref(),
        )
    }

    /// The identity state of one logical request on `root`.
    pub fn state(&self, resolved: &Resolved, root: &str) -> CopilotRequestState {
        let grant = CopilotGrant::of(resolved);
        let key = identity::cache_key(
            grant.enterprise.as_deref(),
            grant.endpoint.as_deref(),
            root,
            &grant.token,
        );
        CopilotRequestState::begin(key, identity::integration_pin(), grant.enterprise.is_some())
    }

    /// Route headers after the transport's content type and bearer.
    pub fn route_headers(&self, messages: &[Message], session: Option<&str>, out: &mut HeaderMap) {
        if self.route == Route::Messages {
            put(out, "accept", "text/event-stream");
            put(out, "anthropic-dangerous-direct-browser-access", "true");
        } else {
            put(out, "accept", "text/event-stream");
        }
        api_headers(out);
        dynamic_headers(messages, out);
        match self.route {
            Route::Messages => {
                if let Some(value) = session
                    .filter(|s| !s.is_empty())
                    .and_then(|s| HeaderValue::from_str(s).ok())
                {
                    out.insert("x-claude-code-session-id", value);
                }
            }
            Route::Responses | Route::Chat => put(out, "x-stainless-timeout", STAINLESS_TIMEOUT),
        }
    }
}

impl RequestProfile for CopilotProfile {
    fn base_url(&self, resolved: &Resolved) -> Result<String, ProviderError> {
        Ok(self.root(resolved))
    }
    fn bearer<'r>(&self, resolved: &'r Resolved) -> Result<Cow<'r, SecretString>, ProviderError> {
        Ok(match CopilotGrant::of(resolved).token {
            // The stored token or a raw env token: the resolved secret itself.
            Cow::Borrowed(_) => Cow::Borrowed(resolved.auth.bearer_secret()),
            // An env JSON envelope: only its token is ever sent.
            Cow::Owned(token) => Cow::Owned(SecretString::new(token)),
        })
    }
    fn headers(&self, _resolved: &Resolved, ctx: &RequestContext<'_>, out: &mut HeaderMap) {
        self.route_headers(ctx.messages, ctx.options.session_id.as_deref(), out);
    }
    /// The identity state is keyed by the root this attempt set already uses.
    fn begin(
        &self,
        resolved: &Resolved,
        base: &str,
        _ctx: &RequestContext<'_>,
    ) -> Option<Box<dyn RequestState>> {
        Some(Box::new(self.state(resolved, base)))
    }
    fn error(
        &self,
        status: u16,
        headers: &HeaderMap,
        body: &str,
        sent: &HeaderMap,
    ) -> Option<ProviderError> {
        Some(http_error(status, headers, body, sent))
    }
    /// Messages: none — omp pins the client retries to 0 on the streaming
    /// Messages path; its retries are the `MESSAGES` provider loop.
    fn retry(&self) -> Option<&'static HttpRetryPolicy> {
        match self.route {
            Route::Chat => Some(&OPENAI_WIRE),
            Route::Messages | Route::Responses => None,
        }
    }
    fn resilience(&self) -> Option<&'static StreamResilience> {
        match self.route {
            Route::Chat => Some(&CHAT),
            Route::Messages => Some(&MESSAGES),
            Route::Responses => None,
        }
    }
}

impl ChatProfile for CopilotProfile {
    /// `store: false` only for a seed row that supports it and was not served by
    /// the request credential's own last live discovery; image parts for vision
    /// models.
    fn compat(&self, resolved: &Resolved, ctx: &RequestContext<'_>) -> ChatCompat {
        let model = ctx.options.model.as_str();
        let store_false = !self.shared.is_live(resolved, model)
            && catalog::seed_row(model).is_some_and(|row| row.store);
        let vision = self
            .shared
            .descriptor(model)
            .is_some_and(|d| d.capabilities.contains(&ModelCapability::Vision));
        ChatCompat {
            store_false,
            max_completion_tokens: (ctx.options.max_tokens > 0).then_some(ctx.options.max_tokens),
            images: if vision {
                ChatImages::Parts
            } else {
                ChatImages::Omitted
            },
            anthropic_cache_control: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{AuthSource, CredentialRouting};

    fn oauth(token: &str, enterprise: Option<&str>, endpoint: Option<&str>) -> Resolved {
        Resolved {
            source: AuthSource::StoredOAuth,
            auth: ResolvedAuth::OAuth {
                access: token.to_owned().into(),
                account_id: None,
                project_id: None,
                routing: CredentialRouting {
                    enterprise_domain: enterprise.map(str::to_owned),
                    api_endpoint: endpoint.map(str::to_owned),
                },
            },
            row: None,
            identity: None,
        }
    }

    fn key(raw: &str) -> Resolved {
        Resolved {
            source: AuthSource::Env {
                var: "COPILOT_GITHUB_TOKEN".into(),
            },
            auth: ResolvedAuth::ApiKey(raw.to_owned().into()),
            row: None,
            identity: None,
        }
    }

    #[test]
    #[serial_test::serial]
    fn root_precedence_follows_omp_with_the_host_suffix_tightening() {
        // SAFETY: serial; the rebasing knob must be absent for literal roots.
        let saved = std::env::var_os("RUSTAIN_TEST_AUTH_ORIGIN");
        unsafe { std::env::remove_var("RUSTAIN_TEST_AUTH_ORIGIN") };
        let endpoint = Some("https://api.business.githubcopilot.com");
        let cases: [(Option<&str>, Resolved, Option<&str>, &str); 9] = [
            // The stored endpoint wins over an absent or managed base.
            (
                None,
                oauth("t", None, endpoint),
                None,
                "https://api.business.githubcopilot.com",
            ),
            (
                Some("https://api.githubcopilot.com"),
                oauth("t", Some("company.ghe.com"), endpoint),
                None,
                "https://api.business.githubcopilot.com",
            ),
            // A configured foreign base survives every credential shape.
            (
                Some("https://proxy.example/v1/"),
                oauth("t", None, endpoint),
                None,
                "https://proxy.example/v1",
            ),
            (
                Some("https://proxy.example"),
                oauth("t", Some("company.ghe.com"), None),
                None,
                "https://proxy.example",
            ),
            // A substring is not a host: not managed.
            (
                Some("https://evil.com/githubcopilot.com"),
                oauth("t", None, endpoint),
                None,
                "https://evil.com/githubcopilot.com",
            ),
            // Enterprise without an endpoint → the Enterprise root.
            (
                None,
                oauth("t", Some("company.ghe.com"), None),
                None,
                "https://copilot-api.company.ghe.com",
            ),
            // Nothing → the learned reprobed root, else canonical.
            (
                None,
                oauth("t", None, None),
                Some("https://api.individual.githubcopilot.com"),
                "https://api.individual.githubcopilot.com",
            ),
            (
                None,
                oauth("t", None, None),
                None,
                "https://api.githubcopilot.com",
            ),
            // An env envelope routes like a stored row.
            (
                None,
                key(
                    r#"{"token":"t","enterpriseUrl":"Company.GHE.com","apiEndpoint":"https://x.githubcopilot.com/p/"}"#,
                ),
                None,
                "https://x.githubcopilot.com/p",
            ),
        ];
        for (configured, resolved, learned, want) in cases {
            let grant = CopilotGrant::of(&resolved);
            assert_eq!(
                copilot_root(configured, &grant, learned),
                want,
                "{configured:?} {learned:?}"
            );
        }
        // SAFETY: serial.
        unsafe {
            if let Some(value) = saved {
                std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", value);
            }
        }
    }

    #[test]
    fn envelope_bearer_is_the_token_alone() {
        let resolved = key(r#"{"token":"tok","apiEndpoint":"https://x.githubcopilot.com"}"#);
        let grant = CopilotGrant::of(&resolved);
        assert_eq!(grant.token, "tok");
        let raw = key("gho_raw");
        assert_eq!(CopilotGrant::of(&raw).token, "gho_raw");
    }

    #[test]
    fn initiator_and_vision_follow_the_last_and_any_user_message() {
        let message = |role, tool: bool, image: bool| Message {
            role,
            content: "x".into(),
            images: if image {
                vec![crate::domain::models::ImageAttachment {
                    media_type: "image/png".into(),
                    data: "AA==".into(),
                }]
            } else {
                vec![]
            },
            tool_results: if tool {
                vec![crate::domain::models::ToolResultMessage {
                    tool_use_id: "c".into(),
                    content: "r".into(),
                    is_error: false,
                    images: vec![],
                }]
            } else {
                vec![]
            },
            tool_uses: vec![],
            context_prefix: None,
            reasoning_content: None,
            provider_replay: vec![],
        };
        assert_eq!(initiator(&[]), "user");
        assert_eq!(
            initiator(&[message(MessageRole::User, false, false)]),
            "user"
        );
        assert_eq!(
            initiator(&[message(MessageRole::Assistant, false, false)]),
            "agent"
        );
        assert_eq!(
            initiator(&[message(MessageRole::User, true, false)]),
            "agent"
        );
        assert!(!vision(&[message(MessageRole::Assistant, false, true)]));
        assert!(vision(&[
            message(MessageRole::User, false, true),
            message(MessageRole::Assistant, false, false)
        ]));
        assert!(!vision(&[message(MessageRole::User, true, false)]));
        let mut tool_image = message(MessageRole::User, true, false);
        tool_image.tool_results[0].images = vec![crate::domain::models::ImageAttachment {
            media_type: "image/png".into(),
            data: "AA==".into(),
        }];
        assert!(vision(&[tool_image]), "a tool-result image is vision input");
    }

    #[test]
    fn errors_are_branded_redacted_and_403_is_never_authentication() {
        let mut sent = HeaderMap::new();
        sent.insert(
            "authorization",
            HeaderValue::from_static("Bearer gho_SENTTOKEN123"),
        );
        let none = HeaderMap::new();
        assert!(matches!(
            http_error(401, &none, "", &sent),
            ProviderError::AuthenticationFailed
        ));
        match http_error(403, &none, "", &sent) {
            ProviderError::Other(text) => assert_eq!(text, FORBIDDEN),
            other => panic!("{other:?}"),
        }
        let mut retry = HeaderMap::new();
        retry.insert("retry-after", HeaderValue::from_static("120"));
        assert!(matches!(
            http_error(429, &retry, "", &sent),
            ProviderError::RateLimited {
                retry_after_ms: Some(120_000)
            }
        ));
        match http_error(
            500,
            &none,
            r#"{"error":{"message":"boom Bearer gho_SENTTOKEN123"}}"#,
            &sent,
        ) {
            ProviderError::ConnectionFailed(text) => {
                assert_eq!(text, "GitHub Copilot server error 500: boom [redacted]");
            }
            other => panic!("{other:?}"),
        }
        match http_error(
            422,
            &none,
            &format!("{} gho_SENTTOKEN123", "x".repeat(10)),
            &sent,
        ) {
            ProviderError::Other(text) => {
                assert!(text.starts_with("GitHub Copilot HTTP 422: "));
                assert!(!text.contains("SENTTOKEN"));
            }
            other => panic!("{other:?}"),
        }
        match http_error(400, &none, r#"{"error":"plain"}"#, &sent) {
            ProviderError::Other(text) => assert_eq!(text, "GitHub Copilot HTTP 400: plain"),
            other => panic!("{other:?}"),
        }
        // Prose about tokens is not a credential; a token-length value is.
        let prose = r#"{"error":{"message":"prompt token count of 9 exceeds the limit of 8"}}"#;
        let ProviderError::Other(text) = http_error(400, &none, prose, &sent) else {
            panic!("a 400 maps to Other");
        };
        assert_eq!(
            text,
            "GitHub Copilot HTTP 400: prompt token count of 9 exceeds the limit of 8"
        );
        assert_eq!(
            redact("Authorization: token abcdefghij0123456789xyz", &none),
            "Authorization: [redacted]"
        );
    }
}
