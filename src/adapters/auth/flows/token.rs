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

use super::FlowError;
use crate::domain::models::{CredentialIdentity, SecretString, auth_policy::*};
use base64::Engine;

/// Login, refresh and bootstrap requests carry codes, verifiers and tokens. A 307/308
/// would resend them to another origin, so their client never follows a redirect.
/// Build failure matches `reqwest::Client::new()` (TLS backend initialization).
pub(super) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client TLS backend initializes")
}

#[derive(Default)]
pub(super) struct Values<'a> {
    pub client_id: &'a str,
    pub redirect: &'a str,
    pub scope: &'a str,
    pub state: &'a str,
    pub challenge: &'a str,
    pub code: &'a str,
    pub verifier: &'a str,
    pub refresh: &'a str,
    pub device_code: &'a str,
}
impl Values<'_> {
    pub fn render(&self, tpl: Tpl) -> &str {
        match tpl {
            Tpl::Lit(v) => v,
            Tpl::ClientId => self.client_id,
            Tpl::RedirectUri => self.redirect,
            Tpl::Scope => self.scope,
            Tpl::State => self.state,
            Tpl::CodeChallenge => self.challenge,
            Tpl::Code => self.code,
            Tpl::CodeVerifier => self.verifier,
            Tpl::RefreshToken => self.refresh,
        }
    }
}
pub(super) fn client_id(id: ClientId) -> Result<String, FlowError> {
    match id {
        ClientId::Literal(id) => Ok(id.to_owned()),
        ClientId::Base64(id) => base64::engine::general_purpose::STANDARD
            .decode(id)
            .ok()
            .and_then(|id| String::from_utf8(id).ok())
            .ok_or_else(|| FlowError::configuration("Invalid OAuth client configuration")),
    }
}
pub(super) struct TokenFailure {
    pub status: Option<u16>,
    pub body: String,
}
impl TokenFailure {
    pub fn safe_id(&self) -> Option<String> {
        let id = serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| {
                        e.as_str()
                            .or_else(|| e.get("type").and_then(|v| v.as_str()))
                    })
                    .map(str::to_owned)
            });
        id.filter(|id| {
            matches!(
                id.as_str(),
                "invalid_grant"
                    | "invalid_token"
                    | "unauthorized_client"
                    | "access_denied"
                    | "invalid_request"
            )
        })
    }
    pub fn safe_message(&self, prefix: &str) -> String {
        let id = self.safe_id();
        match (id, self.status) {
            (Some(id), _) => format!("{prefix}: {id}"),
            (None, Some(status)) => format!("{prefix} (HTTP {status})"),
            _ => format!("{prefix}: transport unavailable"),
        }
    }
}
/// Grant fields in each grant's exact wire order; policy `extra_params` follow them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Grant {
    /// `grant_type, client_id, code, redirect_uri, code_verifier` (browser exchange).
    AuthorizationCode,
    /// `grant_type=authorization_code, client_id, code, code_verifier, redirect_uri`.
    CodexDeviceExchange,
    /// `grant_type, client_id, refresh_token`.
    Refresh,
    /// `grant_type=urn:ietf:params:oauth:grant-type:device_code, client_id, device_code`.
    DeviceCode,
}
impl Grant {
    fn fields<'a>(self, values: &Values<'a>) -> Vec<(&'static str, &'a str)> {
        match self {
            Self::AuthorizationCode => vec![
                ("grant_type", "authorization_code"),
                ("client_id", values.client_id),
                ("code", values.code),
                ("redirect_uri", values.redirect),
                ("code_verifier", values.verifier),
            ],
            Self::CodexDeviceExchange => vec![
                ("grant_type", "authorization_code"),
                ("client_id", values.client_id),
                ("code", values.code),
                ("code_verifier", values.verifier),
                ("redirect_uri", values.redirect),
            ],
            Self::Refresh => vec![
                ("grant_type", "refresh_token"),
                ("client_id", values.client_id),
                ("refresh_token", values.refresh),
            ],
            Self::DeviceCode => vec![
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", values.client_id),
                ("device_code", values.device_code),
            ],
        }
    }
}
/// A token endpoint reply. Only `Grant::DeviceCode` returns non-2xx statuses here (the
/// poll classifies them); its unparsable bodies become `Value::Null`.
pub(super) struct TokenReply {
    pub status: u16,
    pub body: serde_json::Value,
}
/// Resolves a policy URL for one operation. The result is the logical endpoint, which
/// `request` rebases like every other auth endpoint; a discovered endpoint is validated
/// before it is returned. `Err` carries an operator-safe message.
pub(super) async fn resolve_url(http: &reqwest::Client, url: TokenUrl) -> Result<String, String> {
    match url {
        TokenUrl::Static(url) => Ok(url.to_owned()),
        TokenUrl::XaiOidc => crate::adapters::auth::xai_oidc::token_endpoint(http).await,
    }
}
pub(super) async fn request(
    http: &reqwest::Client,
    policy: &TokenRequestPolicy,
    url: &str,
    grant: Grant,
    values: &Values<'_>,
) -> Result<TokenReply, TokenFailure> {
    let mut fields = grant.fields(values);
    fields.extend(
        policy
            .extra_params
            .iter()
            .map(|(key, tpl)| (*key, values.render(*tpl))),
    );
    let mut request = http
        .post(crate::adapters::auth::policies::endpoint(url))
        .timeout(std::time::Duration::from_millis(policy.timeout_ms));
    for (name, tpl) in policy.headers {
        request = request.header(*name, values.render(*tpl));
    }
    request = match policy.encoding {
        BodyEncoding::Json => request.json(&Ordered(&fields)),
        BodyEncoding::Form => request
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(
                url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(fields)
                    .finish(),
            ),
    };
    let response = request.send().await.map_err(|_| TokenFailure {
        status: None,
        body: String::new(),
    })?;
    let status = response.status();
    if grant == Grant::DeviceCode {
        let body = response.text().await.unwrap_or_default();
        return Ok(TokenReply {
            status: status.as_u16(),
            body: serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
        });
    }
    let body = response.text().await.map_err(|_| TokenFailure {
        status: Some(status.as_u16()),
        body: String::new(),
    })?;
    if !status.is_success() {
        return Err(TokenFailure {
            status: Some(status.as_u16()),
            body,
        });
    }
    serde_json::from_str(&body)
        .map(|body| TokenReply {
            status: status.as_u16(),
            body,
        })
        .map_err(|_| TokenFailure {
            status: Some(status.as_u16()),
            body: String::new(),
        })
}
/// JSON object in policy order; omp sends grant fields as written, not sorted.
struct Ordered<'a>(&'a [(&'static str, &'a str)]);
impl serde::Serialize for Ordered<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}
pub(super) struct Projected {
    pub access: SecretString,
    pub refresh: SecretString,
    pub expires_at_ms: i64,
    pub identity: CredentialIdentity,
}
pub(super) fn project(
    value: &serde_json::Value,
    policy: &TokenProjection,
    now: i64,
) -> Result<Projected, FlowError> {
    let access = string(value, Some(policy.access))
        .ok_or_else(|| FlowError::configuration("OAuth token response is missing access_token"))?;
    let expires = value
        .pointer(policy.expires_in)
        .and_then(serde_json::Value::as_f64)
        .filter(|v| v.is_finite())
        .ok_or_else(|| FlowError::configuration("OAuth token response has invalid expires_in"))?;
    let delta = expires * 1000.0;
    if delta < i64::MIN as f64 || delta > i64::MAX as f64 {
        return Err(FlowError::configuration(
            "OAuth token response has invalid expires_in",
        ));
    }
    let expires_at_ms = now
        .checked_add(delta as i64)
        .and_then(|v| v.checked_sub(policy.skew_ms))
        .ok_or_else(|| FlowError::configuration("OAuth token response has invalid expires_in"))?;
    Ok(Projected {
        access: SecretString::new(access),
        refresh: SecretString::new(string(value, Some(policy.refresh)).unwrap_or_default()),
        expires_at_ms,
        identity: CredentialIdentity {
            account_id: string(value, policy.account_id),
            email: string(value, policy.email),
            project_id: string(value, policy.project_id),
            org_id: string(value, policy.org_id),
            org_name: string(value, policy.org_name),
        },
    })
}
fn string(value: &serde_json::Value, path: Option<&str>) -> Option<String> {
    value
        .pointer(path?)
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .map(str::to_owned)
}
/// The refresh token is exposed only to construct its outbound token request.
pub(super) fn refresh_secret(secret: &SecretString) -> &str {
    secret.expose_secret()
}
