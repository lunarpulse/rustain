//! Copilot client-identity fallback and the learned working identity per
//! credential, adapted from oh-my-pi ai/src/providers/github-copilot-headers.ts
//! (`wrapFetchForCopilotFallback`, the working-integration LRU).
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

use std::{num::NonZeroUsize, sync::LazyLock};

use lru::LruCache;
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::adapters::provider::request_profile::{Alternate, RequestState};

/// The Copilot CLI surface (also the Enterprise default).
pub const CLI_INTEGRATION_ID: &str = "copilot-developer-cli";
/// The chat surface, the public default.
pub const CHAT_INTEGRATION_ID: &str = "copilot-chat";
const HEADER: &str = "copilot-integration-id";
/// omp `COPILOT_WORKING_INTEGRATION_CACHE_LIMIT`.
const CAPACITY: usize = 50;

/// An identity that cleared the identity gate for a credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    Cli,
    Chat,
}

impl Identity {
    pub fn id(self) -> &'static str {
        match self {
            Self::Cli => CLI_INTEGRATION_ID,
            Self::Chat => CHAT_INTEGRATION_ID,
        }
    }
}

/// Process-wide learned identities, bounded like omp's.
pub struct IdentityCache(Mutex<LruCache<String, Identity>>);

impl IdentityCache {
    fn new(capacity: usize) -> Self {
        Self(Mutex::new(LruCache::new(
            NonZeroUsize::new(capacity).expect("non-zero capacity"),
        )))
    }
    pub fn get(&self, key: &str) -> Option<Identity> {
        self.0.lock().get(key).copied()
    }
    fn remember(&self, key: String, identity: Identity) {
        self.0.lock().put(key, identity);
    }
    fn clear(&self, key: &str) {
        self.0.lock().pop(key);
    }
}

pub static CACHE: LazyLock<IdentityCache> = LazyLock::new(|| IdentityCache::new(CAPACITY));

/// omp `normalizeCopilotIntegrationId`: trimmed, non-blank, no CR/LF.
pub fn normalize_integration_id(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty() && !trimmed.contains(['\r', '\n'])).then(|| trimmed.to_owned())
}

/// The operator's `COPILOT_INTEGRATION_ID` pin, read per request.
pub fn integration_pin() -> Option<String> {
    crate::infrastructure::utils::env_var_trimmed("COPILOT_INTEGRATION_ID")
        .and_then(|value| normalize_integration_id(&value))
}

/// omp `normalizeCopilotCacheBaseUrl`: trimmed, trailing slashes removed, a
/// parsable URL as `scheme://lowercase-host[:port]path[?query][#fragment]`.
fn normalize_cache_base(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    match url::Url::parse(trimmed) {
        Ok(url) => {
            let mut out = format!("{}://", url.scheme());
            if let Some(host) = url.host_str() {
                out.push_str(&host.to_ascii_lowercase());
            }
            if let Some(port) = url.port() {
                out.push(':');
                out.push_str(&port.to_string());
            }
            out.push_str(url.path().trim_end_matches('/'));
            if let Some(query) = url.query() {
                out.push('?');
                out.push_str(query);
            }
            if let Some(fragment) = url.fragment() {
                out.push('#');
                out.push_str(fragment);
            }
            out
        }
        Err(_) => trimmed.to_lowercase(),
    }
}

/// `<enterprise>\0<endpoint>\0<normalized root sent>\0<token digest>`; the raw
/// token never becomes a key.
pub fn cache_key(
    enterprise: Option<&str>,
    endpoint: Option<&str>,
    root: &str,
    token: &str,
) -> String {
    let digest = blake3::hash(token.as_bytes()).to_hex();
    format!(
        "{}\0{}\0{}\0{}",
        enterprise.unwrap_or_default(),
        endpoint.unwrap_or_default(),
        normalize_cache_base(root),
        &digest.as_str()[..16]
    )
}

/// omp `isCopilotIdentityDenied`: 403, or 400 with `error.code` `model_not_supported`.
fn denied(status: u16, body: &[u8]) -> bool {
    status == 403
        || (status == 400
            && serde_json::from_slice::<serde_json::Value>(body)
                .is_ok_and(|v| v["error"]["code"] == "model_not_supported"))
}

/// Identity state of one logical request. `snapshot` is the learned entry read
/// once at `begin`, so a sibling request learning or clearing it mid-flight
/// never changes this request's alternate decision.
#[derive(Debug)]
pub struct CopilotRequestState {
    key: Option<String>,
    pinned: bool,
    snapshot: Option<Identity>,
    sent: String,
}

impl CopilotRequestState {
    /// Inference: `pin → learned → (enterprise ? CLI : chat)`.
    pub fn begin(key: String, pin: Option<String>, enterprise: bool) -> Self {
        let snapshot = CACHE.get(&key);
        let sent = match (&pin, snapshot) {
            (Some(pin), _) => pin.clone(),
            (None, Some(identity)) => identity.id().to_owned(),
            (None, None) if enterprise => CLI_INTEGRATION_ID.to_owned(),
            (None, None) => CHAT_INTEGRATION_ID.to_owned(),
        };
        Self {
            key: Some(key),
            pinned: pin.is_some(),
            snapshot,
            sent,
        }
    }

    /// Model-policy enablement: no cache key, so nothing is learned or cleared.
    pub fn policy(pin: Option<String>) -> Self {
        Self {
            key: None,
            pinned: pin.is_some(),
            snapshot: None,
            sent: pin.unwrap_or_else(|| CHAT_INTEGRATION_ID.to_owned()),
        }
    }

    pub fn sent(&self) -> &str {
        &self.sent
    }

    fn resend(&self, identity: Identity) -> Alternate {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER, HeaderValue::from_static(identity.id()));
        let key = self.key.clone();
        Alternate {
            headers,
            settle: Box::new(move |ok| {
                if let Some(key) = key {
                    if ok {
                        CACHE.remember(key, identity);
                    } else {
                        CACHE.clear(&key);
                    }
                }
            }),
        }
    }
}

impl RequestState for CopilotRequestState {
    fn headers(&self, out: &mut HeaderMap) {
        if let Ok(value) = HeaderValue::from_str(&self.sent) {
            out.insert(HEADER, value);
        }
    }

    /// Chat denied → once as the CLI; a learned CLI start denied → once as chat;
    /// a pin or an Enterprise CLI default without learned provenance → none.
    fn alternate(&mut self, status: u16, body: &[u8], sent: &HeaderMap) -> Option<Alternate> {
        if self.pinned || !denied(status, body) {
            return None;
        }
        let outgoing = sent.get(HEADER).and_then(|v| v.to_str().ok());
        match outgoing {
            Some(CHAT_INTEGRATION_ID) => Some(self.resend(Identity::Cli)),
            Some(CLI_INTEGRATION_ID)
                if self.key.is_some() && self.snapshot == Some(Identity::Cli) =>
            {
                Some(self.resend(Identity::Chat))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(id: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER, HeaderValue::from_static(id));
        headers
    }

    #[test]
    fn cache_key_normalizes_the_root_and_never_holds_the_token() {
        let key = cache_key(
            Some("company.ghe.com"),
            Some("https://api.business.githubcopilot.com"),
            " HTTPS://API.Business.GithubCopilot.com/Tenant/ ",
            "gho_secret_token",
        );
        let parts: Vec<_> = key.split('\0').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "company.ghe.com");
        assert_eq!(parts[2], "https://api.business.githubcopilot.com/Tenant");
        assert_eq!(parts[3].len(), 16);
        assert!(!key.contains("gho_secret_token"));
        assert_eq!(normalize_cache_base("not a url/"), "not a url");
        assert_eq!(
            normalize_cache_base("http://127.0.0.1:9/a/?q=1#f"),
            "http://127.0.0.1:9/a?q=1#f"
        );
        assert_ne!(
            cache_key(None, None, "https://a", "t1"),
            cache_key(None, None, "https://a", "t2")
        );
        assert_ne!(
            cache_key(None, None, "https://a", "t1"),
            cache_key(None, None, "https://b", "t1")
        );
    }

    #[test]
    fn lru_holds_fifty_entries() {
        let cache = IdentityCache::new(CAPACITY);
        for index in 0..=CAPACITY {
            cache.remember(format!("k{index}"), Identity::Cli);
        }
        assert_eq!(cache.get("k0"), None, "the oldest entry is evicted");
        assert_eq!(cache.get("k1"), Some(Identity::Cli));
        assert_eq!(cache.get(&format!("k{CAPACITY}")), Some(Identity::Cli));
    }

    #[test]
    fn integration_ids_are_trimmed_and_single_line() {
        assert_eq!(
            normalize_integration_id(" custom-id ").as_deref(),
            Some("custom-id")
        );
        assert_eq!(normalize_integration_id("   "), None);
        assert_eq!(normalize_integration_id("a\nb"), None);
    }

    #[test]
    fn alternates_follow_the_sent_identity_and_begin_provenance() {
        let model_not_supported = br#"{"error":{"code":"model_not_supported"}}"#;
        // Chat denial → CLI, for 403 and 400 model_not_supported only.
        let mut chat = CopilotRequestState::policy(None);
        assert_eq!(chat.sent(), CHAT_INTEGRATION_ID);
        assert!(
            chat.alternate(403, b"", &sent(CHAT_INTEGRATION_ID))
                .is_some()
        );
        assert!(
            chat.alternate(400, model_not_supported, &sent(CHAT_INTEGRATION_ID))
                .is_some()
        );
        assert!(
            chat.alternate(
                400,
                br#"{"error":{"code":"bad_request"}}"#,
                &sent(CHAT_INTEGRATION_ID)
            )
            .is_none()
        );
        for status in [401, 429, 500] {
            assert!(
                chat.alternate(status, b"", &sent(CHAT_INTEGRATION_ID))
                    .is_none()
            );
        }
        // A pin never alternates.
        let mut pinned = CopilotRequestState::policy(Some("custom-id".into()));
        assert!(pinned.alternate(403, b"", &sent("custom-id")).is_none());
        // An Enterprise CLI default without learned provenance never reverses.
        let mut enterprise = CopilotRequestState::begin("enterprise-key-unit".into(), None, true);
        assert_eq!(enterprise.sent(), CLI_INTEGRATION_ID);
        assert!(
            enterprise
                .alternate(403, b"", &sent(CLI_INTEGRATION_ID))
                .is_none()
        );
    }

    #[test]
    fn settle_learns_on_success_and_clears_on_failure() {
        let key = "settle-key-unit".to_owned();
        let mut first = CopilotRequestState::begin(key.clone(), None, false);
        assert_eq!(first.sent(), CHAT_INTEGRATION_ID);
        (first
            .alternate(403, b"", &sent(CHAT_INTEGRATION_ID))
            .unwrap()
            .settle)(true);
        assert_eq!(CACHE.get(&key), Some(Identity::Cli));
        let mut second = CopilotRequestState::begin(key.clone(), None, false);
        assert_eq!(second.sent(), CLI_INTEGRATION_ID);
        let alternate = second
            .alternate(403, b"", &sent(CLI_INTEGRATION_ID))
            .unwrap();
        assert_eq!(alternate.headers[HEADER], CHAT_INTEGRATION_ID);
        (alternate.settle)(false);
        assert_eq!(CACHE.get(&key), None);
    }
}
