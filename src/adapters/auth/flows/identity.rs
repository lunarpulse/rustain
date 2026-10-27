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
