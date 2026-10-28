//! GitHub Copilot domain, endpoint and API-key envelope rules, adapted from
//! oh-my-pi catalog/src/wire/github-copilot.ts, and the login client choice
//! and Enterprise prompt handling of ai/src/registry/oauth/github-copilot.ts.
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

use std::borrow::Cow;

use super::DEFAULT_BASE_URL;

/// Device-flow client of the minimal-grant OpenCode app (public github.com).
pub const PUBLIC_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
/// The GitHub Copilot CLI client, registered on Enterprise instances.
pub const ENTERPRISE_CLIENT_ID: &str = "Ov23ctDVkRmgkPke0Mmm";

const PUBLIC_HOSTS: [&str; 3] = ["api.github.com", "github.com", "www.github.com"];

/// omp `normalizeDomain`: the hostname of the input read as a URL (scheme
/// `https://` assumed), lowercased and without its port; `None` when blank or
/// unparsable.
pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let url = if trimmed.contains("://") {
        url::Url::parse(trimmed)
    } else {
        url::Url::parse(&format!("https://{trimmed}"))
    }
    .ok()?;
    url.host_str()
        .filter(|host| !host.is_empty())
        .map(str::to_ascii_lowercase)
}

/// omp `isPublicGitHubHost`.
pub fn is_public_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    PUBLIC_HOSTS.contains(&host.as_str())
}

/// omp `normalizeGitHubCopilotEnterpriseDomain`: `None` for blank input or a
/// public GitHub host; an unparsable value is kept lowercased.
pub fn enterprise_domain(input: Option<&str>) -> Option<String> {
    let trimmed = input?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = normalize_domain(trimmed).unwrap_or_else(|| trimmed.to_lowercase());
    (!normalized.is_empty() && !is_public_host(&normalized)).then_some(normalized)
}

/// omp `getGitHubCopilotBaseUrl`: the canonical API root without an Enterprise
/// domain, else `https://copilot-api.<domain>` (a `copilot-api.` host as is).
pub fn enterprise_root(domain: Option<&str>) -> String {
    match enterprise_domain(domain) {
        None => DEFAULT_BASE_URL.to_owned(),
        Some(domain) if domain.starts_with("copilot-api.") => format!("https://{domain}"),
        Some(domain) => format!("https://copilot-api.{domain}"),
    }
}

/// omp `normalizeGitHubCopilotApiEndpoint`: an `https` URL with a host, trailing
/// slashes removed; no host allowlist.
pub fn normalize_api_endpoint(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if !trimmed.starts_with("https://") {
        return None;
    }
    let url = url::Url::parse(trimmed).ok()?;
    (url.scheme() == "https" && url.host_str().is_some_and(|host| !host.is_empty()))
        .then(|| trimmed.trim_end_matches('/').to_owned())
}

/// The login hosts chosen by the Enterprise prompt answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginDomain {
    /// Device and token endpoints are served here.
    pub oauth_host: String,
    /// Stored as the credential's Enterprise domain.
    pub enterprise: Option<String>,
}

impl LoginDomain {
    /// omp `resolveOAuthClientId`.
    pub fn client_id(&self) -> &'static str {
        if is_public_host(&self.oauth_host) {
            PUBLIC_CLIENT_ID
        } else {
            ENTERPRISE_CLIENT_ID
        }
    }
}

/// omp's prompt handling: blank or a public host logs in on github.com; any
/// other parsable value is an Enterprise domain; `None` is an invalid answer.
pub fn login_domain(answer: &str) -> Option<LoginDomain> {
    let normalized = normalize_domain(answer);
    if !answer.trim().is_empty() && normalized.is_none() {
        return None;
    }
    let enterprise = enterprise_domain(normalized.as_deref());
    let oauth_host = match normalized {
        Some(host) if !is_public_host(&host) => host,
        _ => "github.com".to_owned(),
    };
    Some(LoginDomain {
        oauth_host,
        enterprise,
    })
}

/// A `COPILOT_GITHUB_TOKEN` value: a JSON object with a string `token` (plus
/// optional `enterpriseUrl`/`apiEndpoint`), otherwise the whole string.
/// `Debug` never prints the token.
#[derive(PartialEq, Eq)]
pub struct Envelope<'a> {
    pub token: Cow<'a, str>,
    pub enterprise: Option<String>,
    pub endpoint: Option<String>,
}

impl std::fmt::Debug for Envelope<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Envelope")
            .field("token", &"[redacted]")
            .field("enterprise", &self.enterprise)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// omp `parseGitHubCopilotApiKey`.
pub fn parse_api_key(raw: &str) -> Envelope<'_> {
    if let Ok(serde_json::Value::Object(object)) = serde_json::from_str::<serde_json::Value>(raw)
        && let Some(token) = object.get("token").and_then(|v| v.as_str())
    {
        return Envelope {
            token: Cow::Owned(token.to_owned()),
            enterprise: object
                .get("enterpriseUrl")
                .and_then(|v| v.as_str())
                .and_then(|v| enterprise_domain(Some(v))),
            endpoint: object
                .get("apiEndpoint")
                .and_then(|v| v.as_str())
                .and_then(normalize_api_endpoint),
        };
    }
    Envelope {
        token: Cow::Borrowed(raw),
        enterprise: None,
        endpoint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_answers_map_to_hosts_clients_and_roots() {
        // (answer, oauth host, enterprise domain, client id, request root)
        let public = Some((
            "github.com",
            None,
            PUBLIC_CLIENT_ID,
            "https://api.githubcopilot.com",
        ));
        let company = Some((
            "company.ghe.com",
            Some("company.ghe.com"),
            ENTERPRISE_CLIENT_ID,
            "https://copilot-api.company.ghe.com",
        ));
        let cases = [
            ("", public),
            ("  \n", public),
            ("github.com", public),
            ("https://WWW.GitHub.com/x", public),
            ("API.GITHUB.COM", public),
            ("company.ghe.com", company),
            ("https://Company.GHE.com/path", company),
            (
                "copilot-api.company.ghe.com",
                Some((
                    "copilot-api.company.ghe.com",
                    Some("copilot-api.company.ghe.com"),
                    ENTERPRISE_CLIENT_ID,
                    "https://copilot-api.company.ghe.com",
                )),
            ),
            (
                "ghe.local:8443",
                Some((
                    "ghe.local",
                    Some("ghe.local"),
                    ENTERPRISE_CLIENT_ID,
                    "https://copilot-api.ghe.local",
                )),
            ),
            ("foo bar", None),
        ];
        for (answer, want) in cases {
            let got = login_domain(answer).map(|domain| {
                (
                    domain.oauth_host.clone(),
                    domain.enterprise.clone(),
                    domain.client_id(),
                    enterprise_root(domain.enterprise.as_deref()),
                )
            });
            let want = want.map(|(host, enterprise, client, root)| {
                (
                    host.to_owned(),
                    enterprise.map(str::to_owned),
                    client,
                    root.to_owned(),
                )
            });
            assert_eq!(got, want, "{answer:?}");
        }
    }

    #[test]
    fn domain_normalization_keeps_ipv6_brackets_and_drops_ports() {
        assert_eq!(normalize_domain("[::1]:8443").as_deref(), Some("[::1]"));
        assert_eq!(
            normalize_domain("http://Host.Example:80/p").as_deref(),
            Some("host.example")
        );
        assert_eq!(normalize_domain("   "), None);
        assert_eq!(enterprise_domain(Some("www.github.com")), None);
        assert_eq!(
            enterprise_domain(Some("not a url")).as_deref(),
            Some("not a url")
        );
    }

    #[test]
    fn api_endpoints_need_https_and_a_host() {
        let cases = [
            (
                " https://api.business.githubcopilot.com/ ",
                Some("https://api.business.githubcopilot.com"),
            ),
            (
                "https://api.individual.githubcopilot.com/tenant-a//",
                Some("https://api.individual.githubcopilot.com/tenant-a"),
            ),
            ("https://proxy.example", Some("https://proxy.example")),
            ("http://api.example/x", None),
            ("https://", None),
            ("ftp://x", None),
            ("", None),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_api_endpoint(input).as_deref(), want, "{input:?}");
        }
    }

    #[test]
    fn api_key_envelope_or_raw_token() {
        let parsed = parse_api_key(
            r#"{"token":"t","enterpriseUrl":"Company.GHE.com","apiEndpoint":"https://x.githubcopilot.com/p/"}"#,
        );
        assert_eq!(
            parsed,
            Envelope {
                token: Cow::Borrowed("t"),
                enterprise: Some("company.ghe.com".into()),
                endpoint: Some("https://x.githubcopilot.com/p".into()),
            }
        );
        let raw = parse_api_key("gho_plain");
        assert_eq!(raw.token, "gho_plain");
        assert_eq!((raw.enterprise, raw.endpoint), (None, None));
        // Not an object with a string token: the whole value is the token.
        assert_eq!(parse_api_key(r#"{"token":1}"#).token, r#"{"token":1}"#);
        assert_eq!(parse_api_key("[1]").token, "[1]");
        let public =
            parse_api_key(r#"{"token":"t","enterpriseUrl":"github.com","apiEndpoint":"http://x"}"#);
        assert_eq!((public.enterprise, public.endpoint), (None, None));
    }
}
