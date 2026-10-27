//! Codex SSE request identity, adapted from oh-my-pi.
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

use super::session::Snapshot;
use crate::domain::{errors::ProviderError, models::ResolvedAuth};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};

pub const CODEX_ORIGINATOR: &str = "omp";
pub const CODEX_USER_AGENT: &str = "omp/18.12.6";
pub const CODEX_VERSION: &str = "0.159.0";

fn header(headers: &mut HeaderMap, key: &'static str, value: &str) -> Result<(), ProviderError> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| ProviderError::Other("Invalid Codex request header".into()))?;
    if matches!(
        key,
        "chatgpt-account-id" | "x-openai-internal-codex-residency"
    ) {
        value.set_sensitive(true);
    }
    headers.insert(key, value);
    Ok(())
}
/// Consolidated secret exposure: bearer transport header and request-local JWT claims.
/// The models probe sends omp's discovery set, without residency.
pub(super) fn auth_headers(
    auth: &ResolvedAuth,
    residency: bool,
) -> Result<HeaderMap, ProviderError> {
    let secret = auth.bearer_secret().expose_secret();
    if secret.trim().is_empty() {
        return Err(ProviderError::AuthenticationFailed);
    }
    let mut headers = HeaderMap::new();
    let mut bearer = HeaderValue::try_from(format!("Bearer {secret}"))
        .map_err(|_| ProviderError::Other("Invalid Codex credential".into()))?;
    bearer.set_sensitive(true);
    headers.insert("authorization", bearer);
    if let Some(claims) = crate::adapters::auth::codex_jwt::claims(secret) {
        let auth = &claims["https://api.openai.com/auth"];
        if let Some(account) = auth["chatgpt_account_id"]
            .as_str()
            .filter(|v| !v.is_empty())
        {
            header(&mut headers, "chatgpt-account-id", account)?;
        }
        let residency_claims: &[&str] = if residency {
            &["chatgpt_data_residency", "chatgpt_compute_residency"]
        } else {
            &[]
        };
        for key in residency_claims {
            if let Some(value) = auth[*key].as_str().map(str::trim).filter(|v| !v.is_empty()) {
                if !value.eq_ignore_ascii_case("no_constraint") {
                    header(&mut headers, "x-openai-internal-codex-residency", value)?;
                }
                break;
            }
        }
    }
    headers.insert(
        "openai-beta",
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert("originator", HeaderValue::from_static(CODEX_ORIGINATOR));
    headers.insert("version", HeaderValue::from_static(CODEX_VERSION));
    Ok(headers)
}
#[derive(Serialize)]
struct TurnMetadata<'a> {
    installation_id: &'a str,
    session_id: &'a str,
    thread_id: &'a str,
    turn_id: &'a str,
    window_id: &'a str,
    request_kind: &'static str,
    turn_started_at_unix_ms: i64,
}
fn ascii_json(value: &impl Serialize) -> Result<String, ProviderError> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| ProviderError::Other("Could not encode Codex metadata".into()))?;
    let mut result = String::with_capacity(encoded.len());
    use std::fmt::Write;
    for ch in encoded.chars() {
        if ch as u32 >= 0x7f {
            for unit in ch.encode_utf16(&mut [0; 2]) {
                write!(&mut result, "\\u{unit:04x}").expect("String writer");
            }
        } else {
            result.push(ch);
        }
    }
    Ok(result)
}
/// Request-scoped Codex identity. It is fixed once a logical request begins, so the
/// body is encoded once; only headers are rebuilt when auth or turn state changes.
pub(super) struct Profile {
    turn: String,
    cache_key: String,
    metadata: Value,
}
impl Profile {
    pub fn new(session: &Snapshot) -> Result<Self, ProviderError> {
        let install = crate::adapters::install_id::install_id().map_err(|_| {
            ProviderError::Other("Could not initialize Codex installation identity".into())
        })?;
        let turn = ascii_json(&TurnMetadata {
            installation_id: &install,
            session_id: &session.session_id,
            thread_id: &session.thread_id,
            turn_id: &session.turn_id,
            window_id: &session.window_id,
            request_kind: "turn",
            turn_started_at_unix_ms: session.turn_started_at_unix_ms,
        })?;
        let metadata = json!({"x-codex-installation-id":install,"session_id":session.session_id,"thread_id":session.thread_id,"x-codex-window-id":session.window_id,"turn_id":session.turn_id,"x-codex-turn-metadata":turn});
        Ok(Self {
            turn,
            cache_key: session.session_id.clone(),
            metadata,
        })
    }
    pub fn headers(
        &self,
        session: &Snapshot,
        model: &str,
        auth: &ResolvedAuth,
    ) -> Result<HeaderMap, ProviderError> {
        let mut headers = auth_headers(auth, true)?;
        headers.insert("user-agent", HeaderValue::from_static(CODEX_USER_AGENT));
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        header(
            &mut headers,
            "x-codex-routing-hint",
            &format!("model={model}"),
        )?;
        for key in [
            "conversation_id",
            "session_id",
            "x-client-request-id",
            "session-id",
        ] {
            header(&mut headers, key, &session.session_id)?;
        }
        header(&mut headers, "thread-id", &session.thread_id)?;
        header(&mut headers, "x-codex-window-id", &session.window_id)?;
        header(&mut headers, "x-codex-turn-metadata", &self.turn)?;
        if let Some(value) = &session.turn_state {
            header(&mut headers, "x-codex-turn-state", value)?;
        }
        if let Some(value) = &session.models_etag {
            header(&mut headers, "x-models-etag", value)?;
        }
        Ok(headers)
    }
}
impl crate::adapters::responses::WireProfile for Profile {
    fn prompt_cache_key(&self) -> Option<&str> {
        Some(&self.cache_key)
    }
    fn client_metadata(&self) -> Option<&Value> {
        Some(&self.metadata)
    }
    fn replay_provider(&self) -> Option<&str> {
        Some(super::PROVIDER_KIND)
    }
}
