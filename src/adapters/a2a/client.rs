//! Hardened AgentCard HTTP client and verified cache.

use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, LOCATION};

use crate::domain::models::{A2aPeerSpec, TrustTier};

use super::card::{AgentCardView, decode_and_validate};
use super::error::A2aError;
use super::jws::{decode_verifying_key, verify_card};

const MAX_REDIRECTS: usize = 5;
const MAX_CARD_BYTES: usize = 1024 * 1024;
const AGENT_CARD_PATH: &str = ".well-known/agent-card.json";

pub struct A2aClientAdapter {
    client: reqwest::Client,
    base_url_override: Option<String>,
    cached_card: tokio::sync::RwLock<Option<(AgentCardView, TrustTier)>>,
}

impl A2aClientAdapter {
    pub fn new(spec: &A2aPeerSpec, base_url_override: Option<String>) -> Result<Self, A2aError> {
        let base = base_url_override
            .as_deref()
            .unwrap_or_else(|| spec.url.expose_url());
        let parsed = parse_and_validate_url(base)?;
        if let Some(pinned) = spec.pinned_key.as_ref() {
            decode_verifying_key(pinned)?;
        }
        let allow_loopback_http = parsed.scheme() == "http";
        let client = reqwest::Client::builder()
            .https_only(!allow_loopback_http)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent(format!("rustain/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| A2aError::ClientBuild(error.to_string()))?;

        Ok(Self {
            client,
            base_url_override,
            cached_card: tokio::sync::RwLock::new(None),
        })
    }

    pub async fn refresh_agent_card(&self, spec: &A2aPeerSpec) -> Result<(), A2aError> {
        let result = self.fetch_agent_card(spec).await;
        let mut cached = self.cached_card.write().await;
        match result {
            Ok(card) => {
                *cached = Some(card);
                Ok(())
            }
            Err(error) => {
                *cached = None;
                Err(error)
            }
        }
    }

    pub async fn cached_card(&self) -> Option<(AgentCardView, TrustTier)> {
        self.cached_card.read().await.clone()
    }

    async fn fetch_agent_card(
        &self,
        spec: &A2aPeerSpec,
    ) -> Result<(AgentCardView, TrustTier), A2aError> {
        let base = self
            .base_url_override
            .as_deref()
            .unwrap_or_else(|| spec.url.expose_url());
        let url = agent_card_url(base)?;
        let response = self.follow_redirects(url).await?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if !content_type.as_deref().is_some_and(is_json_content_type) {
            return Err(A2aError::UnexpectedContentType { content_type });
        }

        let body = read_capped_body(response, MAX_CARD_BYTES).await?;
        let raw = std::str::from_utf8(&body).map_err(|_| A2aError::InvalidUtf8)?;
        let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
        let trust = spec.trust_tier();
        if let TrustTier::Verified = trust {
            let pinned = spec.pinned_key.as_ref().ok_or(A2aError::InvalidPinnedKey)?;
            verify_card(raw, pinned)?;
        }
        let card = decode_and_validate(raw)?;
        Ok((card, trust))
    }

    async fn follow_redirects(&self, mut url: url::Url) -> Result<reqwest::Response, A2aError> {
        for redirect_count in 0..=MAX_REDIRECTS {
            validate_url(&url)?;
            let response = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(|error| A2aError::Request(error.to_string()))?;
            if response.status().is_redirection() {
                if redirect_count == MAX_REDIRECTS {
                    return Err(A2aError::TooManyRedirects);
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        A2aError::InvalidRedirect("missing Location header".to_owned())
                    })?;
                url = url
                    .join(location)
                    .map_err(|error| A2aError::InvalidRedirect(error.to_string()))?;
                validate_url(&url)?;
                continue;
            }
            if !response.status().is_success() {
                return Err(A2aError::HttpStatus {
                    status: response.status().as_u16(),
                });
            }
            return Ok(response);
        }
        Err(A2aError::TooManyRedirects)
    }
}

fn agent_card_url(base: &str) -> Result<url::Url, A2aError> {
    let mut url = parse_and_validate_url(base)?;
    let path = url.path().trim_end_matches('/');
    let card_path = if path.is_empty() {
        format!("/{AGENT_CARD_PATH}")
    } else {
        format!("{path}/{AGENT_CARD_PATH}")
    };
    url.set_path(&card_path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn parse_and_validate_url(raw: &str) -> Result<url::Url, A2aError> {
    let url = url::Url::parse(raw).map_err(|error| A2aError::UnsafeUrl {
        reason: error.to_string(),
    })?;
    validate_url(&url)?;
    Ok(url)
}

fn validate_url(url: &url::Url) -> Result<(), A2aError> {
    let host = url.host().ok_or_else(|| A2aError::UnsafeUrl {
        reason: "URL has no host".to_owned(),
    })?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(host) => Ok(()),
        "http" => Err(A2aError::UnsafeUrl {
            reason: "plain HTTP is permitted only for loopback authorities".to_owned(),
        }),
        scheme => Err(A2aError::UnsafeUrl {
            reason: format!("unsupported URL scheme {scheme:?}"),
        }),
    }
}

fn is_loopback_host(host: url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    }
}

fn is_json_content_type(value: &str) -> bool {
    let media_type = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    media_type == "application/json" || media_type.ends_with("+json")
}

async fn read_capped_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, A2aError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| A2aError::Request(error.to_string()))?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(A2aError::BodyTooLarge { max_bytes });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
