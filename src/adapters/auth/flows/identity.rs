//! Portions adapted from NousResearch/hermes-agent, Copyright (c) 2025 Nous Research (MIT)
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

use super::FlowError;
use crate::domain::models::{CredentialIdentity, SecretString, auth_policy::IdentityHook};

pub(super) async fn fill(
    http: &reqwest::Client,
    hook: Option<IdentityHook>,
    access: &SecretString,
    raw: &serde_json::Value,
    identity: &mut CredentialIdentity,
    refresh: bool,
) -> Result<(), FlowError> {
    let Some(hook) = hook else {
        return Ok(());
    };
    // Expose only for the selected identity protocol; never log claims or tokens.
    let access = access.expose_secret();
    match hook {
        IdentityHook::AnthropicBootstrap => bootstrap(http, access, identity, refresh).await,
        IdentityHook::CodexJwt => codex(access, raw, identity, refresh)?,
        IdentityHook::XaiUserinfo => xai(http, access, identity).await,
        // Login only: refresh keeps the stored email through the resolver merge.
        IdentityHook::GoogleUserinfo if !refresh => google(http, access, identity).await,
        IdentityHook::GoogleUserinfo => {}
    }
    Ok(())
}

fn codex(
    access: &str,
    raw: &serde_json::Value,
    identity: &mut CredentialIdentity,
    refresh: bool,
) -> Result<(), FlowError> {
    let access = crate::adapters::auth::codex_jwt::claims(access);
    let id = raw
        .get("id_token")
        .and_then(serde_json::Value::as_str)
        .and_then(crate::adapters::auth::codex_jwt::claims);
    // JavaScript ?? selects the first non-null value BEFORE type/empty filtering.
    let field = |namespace: &str, name: &str| {
        access
            .as_ref()
            .and_then(|claims| claims.get(namespace)?.get(name))
            .filter(|value| !value.is_null())
            .or_else(|| {
                id.as_ref()
                    .and_then(|claims| claims.get(namespace)?.get(name))
            })
    };
    let account = field("https://api.openai.com/auth", "chatgpt_account_id").and_then(|value| {
        value
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    });
    let normalized = |value: Option<&serde_json::Value>| {
        value
            .and_then(|value| value.as_str().map(|value| value.trim().to_lowercase()))
            .filter(|value| !value.is_empty())
    };
    let email = normalized(field("https://api.openai.com/profile", "email"));
    if !refresh && account.is_none() && email.is_none() {
        return Err(FlowError::configuration(
            "Failed to extract account identity from token",
        ));
    }
    identity.account_id = account;
    identity.email = email;
    if !refresh {
        let plan = normalized(field("https://api.openai.com/auth", "chatgpt_plan_type"));
        identity.org_id = identity.account_id.clone();
        identity.org_name = plan;
    }
    // On refresh absent fields stay absent: the resolver retains the stored identity.
    Ok(())
}

async fn bootstrap(
    http: &reqwest::Client,
    access: &str,
    identity: &mut CredentialIdentity,
    refresh: bool,
) {
    if identity.account_id.is_some()
        && identity.email.is_some()
        && (refresh || identity.org_id.is_some())
    {
        return;
    }
    // Expose the access token only at the bootstrap HTTP boundary.
    let response = http.get(crate::adapters::auth::policies::endpoint("https://api.anthropic.com/api/claude_cli/bootstrap?entrypoint=cli&model=claude-opus-4-8"))
        .timeout(std::time::Duration::from_secs(30))
        .header("Accept", "application/json, text/plain, */*")
        .bearer_auth(access)
        .header("Content-Type", "application/json")
        .header("User-Agent",format!("claude-code/{}",crate::adapters::auth::policies::claude_code_version()))
        .header("anthropic-beta","oauth-2025-04-20").send().await;
    let Ok(response) = response else {
        return;
    };
    if !response.status().is_success() {
        return;
    }
    let Ok(value) = response.json::<serde_json::Value>().await else {
        return;
    };
    let Some(account) = value.get("oauth_account") else {
        return;
    };
    let field = |name: &str| {
        account
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned)
    };
    if identity.account_id.is_none() {
        identity.account_id = field("account_uuid");
    }
    if identity.email.is_none() {
        identity.email = field("account_email");
    }
    if !refresh {
        if identity.org_id.is_none() {
            identity.org_id = field("organization_uuid");
        }
        if identity.org_name.is_none() {
            identity.org_name = field("organization_name");
        }
    }
}

/// Same rule on login and refresh: the access-JWT `sub`, then OIDC userinfo. Every
/// userinfo failure keeps the projected identity; on refresh absent fields stay absent,
/// so the resolver retains the stored ones. Never org-scoped.
async fn xai(http: &reqwest::Client, access: &str, identity: &mut CredentialIdentity) {
    const USERINFO: &str = "https://auth.x.ai/oauth2/userinfo";
    if let Some(sub) = crate::adapters::auth::codex_jwt::claims(access)
        .and_then(|claims| scalar_string(claims.get("sub")))
    {
        identity.account_id = Some(sub);
    }
    // Bearer only; the shared credential client never follows a redirect.
    let response = http
        .get(crate::adapters::auth::policies::endpoint(USERINFO))
        .timeout(std::time::Duration::from_secs(15))
        .bearer_auth(access)
        .send()
        .await;
    let Ok(response) = response else {
        return;
    };
    if !response.status().is_success() {
        return;
    }
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return;
    };
    if let Some(email) = scalar_string(body.get("email")) {
        identity.email = Some(email);
    }
    if let Some(sub) = scalar_string(body.get("sub")) {
        identity.account_id = Some(sub);
    }
}

/// omp `fetchUserInfo`: a best-effort bearer GET; any failure leaves identity unset and
/// no body is logged. The 15 s bound is rustain's (omp has only cancellation).
async fn google(http: &reqwest::Client, access: &str, identity: &mut CredentialIdentity) {
    const USERINFO: &str = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
    let response = http
        .get(crate::adapters::auth::policies::endpoint(USERINFO))
        .timeout(std::time::Duration::from_secs(15))
        .bearer_auth(access)
        .send()
        .await;
    let Ok(response) = response else {
        return;
    };
    if !response.status().is_success() {
        return;
    }
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return;
    };
    if let Some(email) = body
        .get("email")
        .and_then(serde_json::Value::as_str)
        .filter(|email| !email.is_empty())
    {
        identity.email = Some(email.to_owned());
    }
}

/// JavaScript `scalarString`: a non-empty string as-is, or a finite number as `String(n)`.
fn scalar_string(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(value) => (!value.is_empty()).then(|| value.clone()),
        serde_json::Value::Number(number) if number.is_f64() => {
            let value = number.as_f64()?;
            value.is_finite().then(|| js_number(value))
        }
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// JavaScript `String(n)` for the finite floats JSON can carry.
fn js_number(value: f64) -> String {
    if value == 0.0 {
        "0".to_owned()
    } else if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::scalar_string;
    use serde_json::json;

    #[test]
    fn scalar_string_follows_javascript_rules() {
        assert_eq!(
            scalar_string(Some(&json!("a@x.ai"))).as_deref(),
            Some("a@x.ai")
        );
        assert_eq!(scalar_string(Some(&json!(" "))).as_deref(), Some(" "));
        assert_eq!(scalar_string(Some(&json!(""))), None);
        assert_eq!(scalar_string(Some(&json!(42))).as_deref(), Some("42"));
        assert_eq!(scalar_string(Some(&json!(42.0))).as_deref(), Some("42"));
        assert_eq!(scalar_string(Some(&json!(1.5))).as_deref(), Some("1.5"));
        assert_eq!(scalar_string(Some(&json!(-0.0))).as_deref(), Some("0"));
        assert_eq!(scalar_string(Some(&json!(null))), None);
        assert_eq!(scalar_string(Some(&json!(true))), None);
        assert_eq!(scalar_string(Some(&json!({"a": 1}))), None);
        assert_eq!(scalar_string(None), None);
    }
}
