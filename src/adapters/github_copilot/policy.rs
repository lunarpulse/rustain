//! Model-policy enablement after a Copilot login, adapted from oh-my-pi
//! (ai/src/registry/oauth/github-copilot.ts `enableGitHubCopilotModel`,
//! `enableAllGitHubCopilotModels`).
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

use reqwest::header::{HeaderMap, HeaderValue};

use super::{catalog, domain::enterprise_root, headers, identity};
use crate::adapters::provider::{http_retry, request_profile::RequestState};

/// Requests sent concurrently; batches run in sequence.
const BATCH: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(10);
const BODY: &str = r#"{"state":"enabled"}"#;

/// The API root policy requests go to: the probed endpoint, else the
/// Enterprise (or canonical) root.
fn root(endpoint: Option<&str>, enterprise: Option<&str>) -> String {
    let root = endpoint.map_or_else(|| enterprise_root(enterprise), str::to_owned);
    crate::adapters::auth::policies::endpoint(&root)
        .trim_end_matches('/')
        .to_owned()
}

/// The policy headers before the integration id: content type, bearer, the
/// API headers with the policy intent, and the user initiator.
fn base_headers(token: &str) -> Option<HeaderMap> {
    let mut out = HeaderMap::new();
    out.insert("content-type", HeaderValue::from_static("application/json"));
    let mut bearer = HeaderValue::try_from(format!("Bearer {token}")).ok()?;
    bearer.set_sensitive(true);
    out.insert("authorization", bearer);
    headers::api_headers(&mut out);
    out.insert("openai-intent", HeaderValue::from_static("chat-policy"));
    out.insert("x-initiator", HeaderValue::from_static("user"));
    out.insert(
        "x-interaction-type",
        HeaderValue::from_static("chat-policy"),
    );
    Some(out)
}

/// Enables every bundled model for the account. Every outcome is ignored: a
/// denied, failed or unreachable request never fails the login.
pub async fn enable_all(
    client: &reqwest::Client,
    token: &str,
    endpoint: Option<&str>,
    enterprise: Option<&str>,
) {
    let Some(base) = base_headers(token) else {
        return;
    };
    let root = root(endpoint, enterprise);
    let pin = identity::integration_pin();
    for batch in catalog::SEED.chunks(BATCH) {
        futures::future::join_all(
            batch
                .iter()
                .map(|row| enable(client, &root, row.id, &base, pin.clone())),
        )
        .await;
    }
}

async fn enable(
    client: &reqwest::Client,
    root: &str,
    id: &str,
    base: &HeaderMap,
    pin: Option<String>,
) {
    let mut state = identity::CopilotRequestState::policy(pin);
    let mut headers = base.clone();
    state.headers(&mut headers);
    let Ok(request) = client
        .post(format!("{root}/models/{id}/policy"))
        .headers(headers)
        .body(BODY)
        .timeout(TIMEOUT)
        .build()
    else {
        return;
    };
    // Outcomes are ignored; the identity alternate still applies once.
    let _ = http_retry::send(client, &request, Some(&mut state), None).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn policy_root_prefers_the_probed_endpoint() {
        // SAFETY: serial; the rebasing knob must be absent for literal roots.
        let saved = std::env::var_os("RUSTAIN_TEST_AUTH_ORIGIN");
        unsafe { std::env::remove_var("RUSTAIN_TEST_AUTH_ORIGIN") };
        assert_eq!(
            root(
                Some("https://api.business.githubcopilot.com/t"),
                Some("company.ghe.com")
            ),
            "https://api.business.githubcopilot.com/t"
        );
        assert_eq!(
            root(None, Some("company.ghe.com")),
            "https://copilot-api.company.ghe.com"
        );
        assert_eq!(root(None, None), "https://api.githubcopilot.com");
        // SAFETY: serial.
        unsafe {
            if let Some(value) = saved {
                std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", value);
            }
        }
    }

    #[test]
    fn policy_headers_override_the_intent_and_add_the_initiator() {
        let headers = base_headers("tok").unwrap();
        assert_eq!(headers["openai-intent"], "chat-policy");
        assert_eq!(headers["x-initiator"], "user");
        assert_eq!(headers["x-interaction-type"], "chat-policy");
        assert_eq!(headers["authorization"], "Bearer tok");
        assert_eq!(headers["x-github-api-version"], "2026-08-01");
    }
}
