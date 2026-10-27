//! xAI OIDC token-endpoint discovery and validation, adapted from oh-my-pi's xAI OAuth
//! helpers. The discovered endpoint receives every device code and refresh token, so it
//! is validated (https, `x.ai` / `*.x.ai`) before any request is sent to it.
//!
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

const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const INVALID_JSON: &str = "xAI OIDC discovery returned invalid JSON";

/// Discovers xAI's token endpoint for one login or one refresh (never cached). Returns
/// the validated logical URL; callers rebase it like every other auth endpoint. `Err`
/// carries an operator-safe message (no transport or parser detail).
pub async fn token_endpoint(http: &reqwest::Client) -> Result<String, String> {
    let response = http
        .get(super::policies::endpoint(DISCOVERY_URL))
        .timeout(DISCOVERY_TIMEOUT)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| "xAI OIDC discovery failed: transport unavailable".to_owned())?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("xAI OIDC discovery returned status {status}."));
    }
    // An unreadable body parses as empty, which is invalid JSON too.
    let body = response.bytes().await.unwrap_or_default();
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Err(INVALID_JSON.to_owned());
    };
    discovered_token_endpoint(&value)
}

/// JavaScript `isRecord` admits arrays, which then fail the `token_endpoint` check.
fn discovered_token_endpoint(value: &serde_json::Value) -> Result<String, String> {
    if !(value.is_object() || value.is_array()) {
        return Err("xAI OIDC discovery response was not a JSON object.".to_owned());
    }
    let endpoint = value
        .get("token_endpoint")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .ok_or_else(|| "xAI OIDC discovery response was missing token_endpoint.".to_owned())?;
    validate_token_endpoint(endpoint)?;
    Ok(endpoint.to_owned())
}

/// Host-family check, not origin pinning: any port on `x.ai` or a `*.x.ai` host.
fn validate_token_endpoint(endpoint: &str) -> Result<(), String> {
    let parsed = url::Url::parse(endpoint).ok();
    let host = parsed
        .as_ref()
        .filter(|parsed| parsed.scheme() == "https")
        .and_then(url::Url::host_str)
        .map(str::to_ascii_lowercase);
    match host {
        Some(host) if host == "x.ai" || host.ends_with(".x.ai") => Ok(()),
        _ => Err(format!("Invalid xAI token_endpoint: {endpoint}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validator_admits_only_https_xai_host_family() {
        for accepted in [
            "https://x.ai/t",
            "https://auth.x.ai/oauth2/token",
            "https://AUTH.X.AI:8443/t",
            "https://a.b.x.ai/t",
        ] {
            assert_eq!(validate_token_endpoint(accepted), Ok(()), "{accepted}");
        }
        for refused in [
            "http://auth.x.ai/t",
            "https://evilx.ai/t",
            "https://x.ai.evil.com/t",
            "https://127.0.0.1/t",
            "https://[::1]/t",
            "https://x.ai./t",
            "ftp://auth.x.ai/t",
            "auth.x.ai/t",
            "not a url",
        ] {
            assert_eq!(
                validate_token_endpoint(refused),
                Err(format!("Invalid xAI token_endpoint: {refused}")),
                "{refused}"
            );
        }
    }

    #[test]
    fn discovery_document_table() {
        let not_object = Err("xAI OIDC discovery response was not a JSON object.".to_owned());
        let missing = Err("xAI OIDC discovery response was missing token_endpoint.".to_owned());
        for value in [json!(null), json!("x"), json!(1), json!(true)] {
            assert_eq!(discovered_token_endpoint(&value), not_object, "{value}");
        }
        for value in [
            json!([]),
            json!({}),
            json!({"token_endpoint": ""}),
            json!({"token_endpoint": "   "}),
            json!({"token_endpoint": 7}),
            json!({"token_endpoint": null}),
        ] {
            assert_eq!(discovered_token_endpoint(&value), missing, "{value}");
        }
        assert_eq!(
            discovered_token_endpoint(&json!({"token_endpoint": " https://auth.x.ai/t "})),
            Ok("https://auth.x.ai/t".to_owned())
        );
        assert_eq!(
            discovered_token_endpoint(&json!({"token_endpoint": "https://evil.example/t"})),
            Err("Invalid xAI token_endpoint: https://evil.example/t".to_owned())
        );
    }
}
