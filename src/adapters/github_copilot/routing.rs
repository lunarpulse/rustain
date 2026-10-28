//! The Copilot routing probe (`copilot_internal/user`), adapted from oh-my-pi
//! (catalog/src/wire/github-copilot.ts `discoverGitHubCopilotApiEndpoint`).
//!
//! MIT License
//!
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

use super::{domain::normalize_api_endpoint, headers::USER_AGENT};

/// Always the public API host, even after an Enterprise login.
const PROBE_URL: &str = "https://api.github.com/copilot_internal/user";
const TIMEOUT: Duration = Duration::from_secs(10);

/// The account's Copilot API endpoint, or `None` on any failure: transport,
/// non-2xx, a non-JSON body, or no usable `endpoints.api`.
pub async fn probe(client: &reqwest::Client, token: &str) -> Option<String> {
    let mut authorization =
        reqwest::header::HeaderValue::try_from(format!("token {token}")).ok()?;
    authorization.set_sensitive(true);
    let response = client
        .get(crate::adapters::auth::policies::endpoint(PROBE_URL))
        .header("accept", "application/json")
        .header("authorization", authorization)
        .header("user-agent", USER_AGENT)
        .timeout(TIMEOUT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    endpoint_of(&body)
}

/// `endpoints.api` of a probe body, normalized.
fn endpoint_of(body: &serde_json::Value) -> Option<String> {
    body.get("endpoints")
        .filter(|endpoints| endpoints.is_object())
        .and_then(|endpoints| endpoints.get("api"))
        .and_then(serde_json::Value::as_str)
        .and_then(normalize_api_endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_an_https_api_endpoint_is_kept() {
        let of = |body| endpoint_of(&body);
        assert_eq!(
            of(json!({"endpoints":{"api":" https://api.business.githubcopilot.com/tenant/ "}}))
                .as_deref(),
            Some("https://api.business.githubcopilot.com/tenant")
        );
        assert_eq!(
            of(json!({"endpoints":{"api":"http://api.example/x"}})),
            None
        );
        assert_eq!(of(json!({"endpoints":{"api":"https://"}})), None);
        assert_eq!(of(json!({"endpoints":{}})), None);
        assert_eq!(of(json!({"endpoints":"x"})), None);
        assert_eq!(of(json!({})), None);
        assert_eq!(of(json!([1])), None);
    }
}
