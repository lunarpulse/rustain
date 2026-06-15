//! GitHub Releases API client for self-update (Story 13.3a).

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;

use crate::adapters::self_update::types::{
    GH_OWNER_REPO, MAX_BINARY_SIZE, ReleaseAsset, ReleaseInfo, TRUSTED_HOST_SUFFIX, TRUSTED_HOSTS,
    UpdateError,
};
use crate::domain::ports::self_update::SelfUpdatePort;

/// Maximum number of redirects before bailing.
const MAX_REDIRECTS: u8 = 5;

/// Maximum text asset size (SHA256SUMS / .minisig): 1 MB.
const MAX_TEXT_SIZE: usize = 1024 * 1024;

/// HTTP client for the GitHub Releases API with channel-pin redirect validation.
pub struct GithubReleaseClient {
    client: Client,
}

impl GithubReleaseClient {
    pub fn new() -> Result<Self, UpdateError> {
        let client = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent(format!("rustain/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| UpdateError::Other(format!("failed to build HTTP client: {e}")))?;
        Ok(Self { client })
    }
}

/// Classify a reqwest error into the self-update error domain.
///
/// Duplicated from `classify_reqwest_error` in the provider module because that
/// function is cfg-gated on anthropic/openai/ollama features which may not be
/// enabled alongside `self-update`.
fn classify_error(e: &reqwest::Error) -> UpdateError {
    if e.is_connect() || e.is_timeout() {
        UpdateError::Offline(e.to_string())
    } else {
        UpdateError::ConnectionFailed(e.to_string())
    }
}

/// Validate that a URL uses HTTPS and its host is in the trusted set (AC7: channel-pin).
fn validate_channel_pin(url: &str) -> Result<(), UpdateError> {
    let parsed = url::Url::parse(url)
        .map_err(|e| UpdateError::UntrustedHost(format!("invalid URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| UpdateError::UntrustedHost("URL has no host".into()))?;
    if parsed.scheme() != "https" {
        return Err(UpdateError::UntrustedHost(format!(
            "non-HTTPS scheme: {}",
            parsed.scheme()
        )));
    }
    // Exact GitHub origins, OR any subdomain of githubusercontent.com (the
    // GitHub-controlled CDN — see TRUSTED_HOST_SUFFIX). The apex itself is
    // included for completeness though assets always use a subdomain.
    let trusted = TRUSTED_HOSTS.contains(&host)
        || host == "githubusercontent.com"
        || host.ends_with(TRUSTED_HOST_SUFFIX);
    if !trusted {
        return Err(UpdateError::UntrustedHost(host.to_string()));
    }
    Ok(())
}

/// Read a response body up to `max` bytes (OOM guard for the metadata path).
async fn read_capped_body(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, UpdateError> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| classify_error(&e))?;
        if buf.len() + chunk.len() > max {
            return Err(UpdateError::ConnectionFailed(format!(
                "response exceeds {max} byte limit"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Follow redirects manually with channel-pin validation, returning the final
/// response. Caps the redirect chain at [`MAX_REDIRECTS`].
async fn follow_redirects(
    client: &Client,
    initial_url: &str,
    headers: &[(&str, &str)],
) -> Result<reqwest::Response, UpdateError> {
    // AC7: pin the INITIAL request before any byte is fetched, then re-pin every redirect.
    validate_channel_pin(initial_url)?;
    let mut url = initial_url.to_string();
    for _ in 0..MAX_REDIRECTS {
        let mut req = client.get(&url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.map_err(|e| classify_error(&e))?;

        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    UpdateError::ConnectionFailed("redirect without Location header".into())
                })?
                .to_string();
            validate_channel_pin(&location)?;
            url = location;
            continue;
        }

        if !status.is_success() {
            return Err(UpdateError::ConnectionFailed(format!(
                "HTTP {status} from {url}"
            )));
        }
        return Ok(resp);
    }
    Err(UpdateError::ConnectionFailed(format!(
        "too many redirects (>{MAX_REDIRECTS}) from {initial_url}"
    )))
}

#[async_trait]
impl SelfUpdatePort for GithubReleaseClient {
    async fn latest_release(&self) -> Result<ReleaseInfo, UpdateError> {
        let url = format!("https://api.github.com/repos/{GH_OWNER_REPO}/releases/latest");
        let resp = follow_redirects(
            &self.client,
            &url,
            &[("Accept", "application/vnd.github+json")],
        )
        .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(UpdateError::ConnectionFailed(format!(
                "GitHub API returned HTTP {status}"
            )));
        }

        // Cap the metadata response (the download paths cap via MAX_*_SIZE; this
        // path must too — an unbounded .json() is an OOM vector on a tampered endpoint).
        let body = read_capped_body(resp, MAX_TEXT_SIZE).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| UpdateError::ConnectionFailed(format!("invalid JSON: {e}")))?;

        let tag = json["tag_name"]
            .as_str()
            .ok_or_else(|| UpdateError::ConnectionFailed("missing tag_name".into()))?;
        let version = tag.strip_prefix('v').unwrap_or(tag).to_string();

        let body = json["body"].as_str().unwrap_or("");
        let notes: String = body.lines().take(5).collect::<Vec<_>>().join("\n");

        let assets = json["assets"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        let name = a["name"].as_str()?.to_string();
                        let download_url = a["browser_download_url"].as_str()?.to_string();
                        Some(ReleaseAsset { name, download_url })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(ReleaseInfo {
            version,
            notes,
            assets,
        })
    }

    async fn download_asset(&self, asset: &ReleaseAsset) -> Result<Vec<u8>, UpdateError> {
        use futures::StreamExt;

        let resp = follow_redirects(&self.client, &asset.download_url, &[]).await?;

        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| classify_error(&e))?;
            if buf.len() + chunk.len() > MAX_BINARY_SIZE {
                return Err(UpdateError::ConnectionFailed(format!(
                    "asset exceeds {MAX_BINARY_SIZE} byte limit"
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    async fn download_text_asset(&self, asset: &ReleaseAsset) -> Result<String, UpdateError> {
        use futures::StreamExt;

        let resp = follow_redirects(&self.client, &asset.download_url, &[]).await?;

        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| classify_error(&e))?;
            if buf.len() + chunk.len() > MAX_TEXT_SIZE {
                return Err(UpdateError::ConnectionFailed(format!(
                    "text asset exceeds {MAX_TEXT_SIZE} byte limit"
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        String::from_utf8(buf)
            .map_err(|e| UpdateError::ConnectionFailed(format!("asset is not valid UTF-8: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC7: GitHub origins + ALL githubusercontent.com subdomains are trusted —
    // including release-assets.githubusercontent.com, the CDN host the exact
    // allowlist missed (caught by the C2 real-swap proof, 2026-06-15).
    #[test]
    fn channel_pin_allows_github_and_cdn_subdomains() {
        for url in [
            "https://github.com/lunarpulse/rustain/releases/download/v0.1.1/rustain-x",
            "https://api.github.com/repos/lunarpulse/rustain/releases/latest",
            "https://release-assets.githubusercontent.com/abc/def",
            "https://objects.githubusercontent.com/abc", // legacy CDN host still ok
        ] {
            assert!(validate_channel_pin(url).is_ok(), "must allow {url}");
        }
    }

    // AC7: non-HTTPS, off-allowlist hosts, and look-alikes must all be rejected.
    #[test]
    fn channel_pin_rejects_untrusted_and_lookalikes() {
        for url in [
            "https://evil.example.com/x",
            "https://github.com.evil.com/x", // suffix attack on github.com
            "https://evilgithubusercontent.com/x", // no leading dot before suffix
            "https://x.githubusercontent.com.evil.com/x", // ends in .evil.com
            "http://github.com/x",           // non-HTTPS
        ] {
            assert!(
                matches!(
                    validate_channel_pin(url),
                    Err(UpdateError::UntrustedHost(_))
                ),
                "must reject {url}"
            );
        }
    }
}
