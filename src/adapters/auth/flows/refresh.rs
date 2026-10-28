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

use super::token::{self, Grant, Values};
use crate::domain::models::auth_policy::Refresh;
use crate::domain::{
    models::OAuthCredential,
    ports::{OAuthRefreshPort, RefreshFailure, RefreshedTokens},
};
use async_trait::async_trait;
use regex::Regex;
use std::sync::LazyLock;

pub struct HttpOAuthRefresher {
    http: reqwest::Client,
}
impl Default for HttpOAuthRefresher {
    fn default() -> Self {
        Self::new()
    }
}
impl HttpOAuthRefresher {
    pub fn new() -> Self {
        Self {
            http: token::client(),
        }
    }
}
#[async_trait]
impl OAuthRefreshPort for HttpOAuthRefresher {
    async fn refresh(
        &self,
        provider: &str,
        credential: &OAuthCredential,
    ) -> Result<RefreshedTokens, RefreshFailure> {
        let policy = match crate::adapters::auth::policies::oauth_api(provider)
            .ok_or_else(|| {
                RefreshFailure::Transient("oauth refresh unavailable for provider".into())
            })?
            .refresh
        {
            Refresh::Http(policy) => policy,
            // omp refresh-same: the stored grant is the access token, so the
            // resolver's fenced write only bumps the revision and lifetime.
            Refresh::StoredGrant { lifetime_ms } => {
                return Ok(RefreshedTokens {
                    access: credential.refresh.clone(),
                    refresh: Some(credential.refresh.clone()),
                    expires_at_ms: crate::infrastructure::utils::auth_now_ms()
                        .saturating_add(lifetime_ms),
                    identity: credential.identity.clone(),
                });
            }
        };
        // Checked before any HTTP. Transient, as omp does not classify it definitive,
        // so the row is never disabled for it.
        if policy.require_project_id && credential.identity.project_id.is_none() {
            return Err(RefreshFailure::Transient(format!(
                "{provider} credentials are missing projectId; sign in again"
            )));
        }
        let client = token::client_id(policy.client_id)
            .map_err(|_| RefreshFailure::Transient("oauth refresh configuration invalid".into()))?;
        let secret = policy
            .client_secret
            .map(token::client_secret)
            .transpose()
            .map_err(|_| RefreshFailure::Transient("oauth refresh configuration invalid".into()))?;
        let values = Values {
            client_id: &client,
            client_secret: secret.as_ref().map(token::grant_secret),
            refresh: token::grant_secret(&credential.refresh),
            ..Default::default()
        };
        // Resolved per refresh: a discovered endpoint is rediscovered and revalidated,
        // and a refused one sends no refresh token anywhere.
        let url = token::resolve_url(&self.http, policy.request.url)
            .await
            .map_err(RefreshFailure::Transient)?;
        let value = token::request(&self.http, policy.request, &url, Grant::Refresh, &values)
            .await
            .map_err(classify)?
            .body;
        let mut projected = token::project(
            &value,
            policy.projection,
            crate::infrastructure::utils::auth_now_ms(),
        )
        .map_err(|_| RefreshFailure::Transient("oauth refresh response invalid".into()))?;
        super::identity::fill(
            &self.http,
            policy.identity_hook,
            &projected.access,
            &value,
            &mut projected.identity,
            true,
        )
        .await
        .map_err(|_| RefreshFailure::Transient("oauth refresh identity invalid".into()))?;
        Ok(RefreshedTokens {
            access: projected.access,
            refresh: (!token::grant_secret(&projected.refresh).is_empty())
                .then_some(projected.refresh),
            expires_at_ms: projected.expires_at_ms,
            identity: projected.identity,
        })
    }
}
fn classify(failure: token::TokenFailure) -> RefreshFailure {
    static DEFINITIVE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)invalid_grant|invalid_token|unauthorized_client|\brevoked\b|refresh[\s_]?token.*expired").expect("valid definitive OAuth regex")
    });
    static TRANSIENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)timeout|network|fetch failed|ECONN(?:REFUSED|RESET)|ETIMEDOUT|ETIMEOUT|ENOTFOUND|ENETUNREACH|EHOSTUNREACH|FailedToOpenSocket|EAI_AGAIN|socket hang up|\b(?:408|425|429|5\d{2})\b|rate.?limit|too many requests|temporar|unavailable|forbidden|permission_denied|cloudflare|captcha").expect("valid transient OAuth regex")
    });
    let message = failure.safe_id().unwrap_or_else(|| {
        failure.status.map_or_else(
            || "transport unavailable".into(),
            |status| format!("HTTP {status}"),
        )
    });
    if failure.status.is_some()
        && (DEFINITIVE.is_match(&failure.body)
            || (failure.status == Some(401) && !TRANSIENT.is_match(&failure.body)))
    {
        RefreshFailure::Definitive(message)
    } else {
        RefreshFailure::Transient(message)
    }
}
