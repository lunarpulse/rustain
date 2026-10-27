//! xAI generic Responses request profile, adapted from oh-my-pi
//! (ai/src/providers/openai-responses.ts, openai-shared.ts, utils/openai-http.ts;
//! output limits from catalog/src/models.json).
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

use super::{XaiKind, errors};
use crate::adapters::responses::{Dialect, WireProfile, transport::Wire};
use crate::domain::{
    errors::ProviderError,
    models::{CompletionOptions, ResolvedAuth},
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;

/// omp's xAI attribution (`utils/src/dirs.ts` `USER_AGENT`, package 18.12.6).
pub const XAI_USER_AGENT: &str = "omp/18.12.6";
/// The first-event timeout in seconds, announced as `X-Stainless-Timeout`.
const STAINLESS_TIMEOUT: &str = "300";
/// omp `OPENAI_MAX_OUTPUT_TOKENS`, the provider clamp of every Responses request.
const OUTPUT_CLAMP: u32 = 64_000;

/// `models.json` `.xai[].maxTokens` of the seeded chat rows.
const XAI_LIMITS: &[(&str, u32)] = &[
    ("grok-2", 8_192),
    ("grok-2-1212", 8_192),
    ("grok-2-latest", 8_192),
    ("grok-2-vision", 4_096),
    ("grok-2-vision-1212", 4_096),
    ("grok-2-vision-latest", 4_096),
    ("grok-3", 8_192),
    ("grok-3-fast", 8_192),
    ("grok-3-fast-latest", 8_192),
    ("grok-3-latest", 8_192),
    ("grok-3-mini", 8_192),
    ("grok-3-mini-fast", 8_192),
    ("grok-3-mini-fast-latest", 8_192),
    ("grok-3-mini-latest", 8_192),
    ("grok-4", 64_000),
    ("grok-4-1-fast", 30_000),
    ("grok-4-1-fast-non-reasoning", 30_000),
    ("grok-4-fast", 30_000),
    ("grok-4-fast-non-reasoning", 30_000),
    ("grok-4.20-0309-non-reasoning", 30_000),
    ("grok-4.20-0309-reasoning", 30_000),
    ("grok-4.20-beta-latest-non-reasoning", 30_000),
    ("grok-4.20-beta-latest-reasoning", 30_000),
    ("grok-4.20-multi-agent-beta-latest", 30_000),
    ("grok-4.3", 30_000),
    ("grok-4.5", 500_000),
    ("grok-4.6", 500_000),
    ("grok-4.7", 500_000),
    ("grok-beta", 4_096),
    ("grok-build-0.1", 256_000),
    ("grok-code-fast-1", 10_000),
    ("grok-vision-beta", 4_096),
];

/// `models.json` `.xai-oauth[].maxTokens` of the curated chat rows.
const XAI_OAUTH_LIMITS: &[(&str, u32)] = &[
    ("grok-build", 512_000),
    ("grok-build-0.1", 256_000),
    ("grok-4.3", 1_000_000),
    ("grok-4.5", 500_000),
    ("grok-4.6", 500_000),
    ("grok-4.7", 500_000),
    ("grok-4.20-multi-agent-0309", 1_000_000),
    ("grok-4.20-0309-reasoning", 1_000_000),
    ("grok-4.20-0309-non-reasoning", 1_000_000),
    ("grok-composer-2.5-fast", 200_000),
];

/// `min(requested, model limit, 64 000)` when a positive limit is requested;
/// ids without a table row (live-only rows) have no model limit.
pub(super) fn max_output_tokens(kind: XaiKind, model: &str, requested: u32) -> Option<u32> {
    if requested == 0 {
        return None;
    }
    let table = match kind {
        XaiKind::Xai => XAI_LIMITS,
        XaiKind::XaiOauth => XAI_OAUTH_LIMITS,
    };
    let limit = table
        .iter()
        .find(|(id, _)| *id == model)
        .map_or(u32::MAX, |(_, limit)| *limit);
    Some(requested.min(limit).min(OUTPUT_CLAMP))
}

/// Consolidated secret exposure: the bearer transport header of every xAI request.
pub(super) fn bearer(auth: &ResolvedAuth) -> Result<HeaderValue, ProviderError> {
    let secret = auth.bearer_secret().expose_secret();
    if secret.trim().is_empty() {
        return Err(ProviderError::AuthenticationFailed);
    }
    let mut value = HeaderValue::try_from(format!("Bearer {secret}"))
        .map_err(|_| ProviderError::Other("Invalid xAI credential".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

/// Discovery and probe headers: the bearer only (callers add `accept`).
pub(super) fn auth_headers(auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", bearer(auth)?);
    Ok(headers)
}

/// Request-scoped xAI policy; fixed for the logical request, so the body is
/// encoded once.
pub(super) struct Profile {
    kind: XaiKind,
    cache_key: Option<String>,
    reasoning: bool,
    vision: bool,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
}

impl Profile {
    /// `reasoning`/`vision` come from the served catalogue row of `options.model`
    /// (`false` for an id no row describes).
    pub(super) fn new(
        kind: XaiKind,
        options: &CompletionOptions,
        reasoning: bool,
        vision: bool,
    ) -> Self {
        Self {
            kind,
            // omp omits the key and its header without a session; no fallback id.
            cache_key: options
                .session_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(crate::adapters::responses::prompt_cache_key),
            reasoning,
            vision,
            max_output_tokens: max_output_tokens(kind, &options.model, options.max_tokens),
            temperature: options.temperature,
        }
    }
}

impl WireProfile for Profile {
    fn dialect(&self) -> Dialect {
        Dialect::Generic
    }
    fn prompt_cache_key(&self) -> Option<&str> {
        self.cache_key.as_deref()
    }
    fn client_metadata(&self) -> Option<&Value> {
        None
    }
    fn replay_provider(&self) -> Option<&str> {
        Some(self.kind.id())
    }
    fn reasoning(&self) -> bool {
        self.reasoning
    }
    fn vision(&self) -> bool {
        self.vision
    }
    fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }
    fn temperature(&self) -> Option<f32> {
        self.temperature
    }
    fn reject_root_object_union(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl Wire for Profile {
    /// omp's order: content type, accept, conversation id, user agent, bearer, timeout.
    fn headers(&self, _model: &str, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        if let Some(key) = &self.cache_key {
            let value = HeaderValue::from_str(key)
                .map_err(|_| ProviderError::Other("Invalid xAI request header".into()))?;
            headers.insert("x-grok-conv-id", value);
        }
        headers.insert("user-agent", HeaderValue::from_static(XAI_USER_AGENT));
        headers.insert("authorization", bearer(auth)?);
        headers.insert(
            "x-stainless-timeout",
            HeaderValue::from_static(STAINLESS_TIMEOUT),
        );
        Ok(headers)
    }
    fn http_error(
        &self,
        status: u16,
        headers: &HeaderMap,
        body: &str,
        request: &HeaderMap,
    ) -> ProviderError {
        errors::http_error(status, headers, body, request)
    }
    fn redact(&self, message: &str, request: &HeaderMap) -> String {
        errors::redact(message, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::SecretString;

    fn options(session: Option<&str>, model: &str, max_tokens: u32) -> CompletionOptions {
        CompletionOptions {
            session_id: session.map(str::to_owned),
            model: model.into(),
            max_tokens,
            system_prompt: String::new(),
            temperature: Some(0.5),
            tools: Vec::new(),
        }
    }

    #[test]
    fn output_limit_is_min_of_request_model_row_and_provider_clamp() {
        let cases = [
            (XaiKind::Xai, "grok-2-vision", 100_000, Some(4_096)),
            (XaiKind::Xai, "grok-code-fast-1", 100_000, Some(10_000)),
            (XaiKind::Xai, "grok-4.3", 50_000, Some(30_000)),
            (XaiKind::Xai, "grok-4.6", 100_000, Some(64_000)),
            (XaiKind::Xai, "grok-4.6", 1_000, Some(1_000)),
            (XaiKind::Xai, "grok-live-only", 100_000, Some(64_000)),
            (XaiKind::Xai, "grok-4.6", 0, None),
            (XaiKind::XaiOauth, "grok-4.3", 50_000, Some(50_000)),
            (XaiKind::XaiOauth, "grok-4.3", 100_000, Some(64_000)),
        ];
        for (kind, model, requested, expected) in cases {
            assert_eq!(max_output_tokens(kind, model, requested), expected);
        }
        assert!(XAI_OAUTH_LIMITS.iter().all(|(_, limit)| *limit >= 200_000));
    }

    #[test]
    fn headers_follow_omp_order_and_omit_the_conversation_id_without_session() {
        let auth = ResolvedAuth::ApiKey(SecretString::new("xai-key".into()));
        let with = Profile::new(
            XaiKind::Xai,
            &options(Some("conv"), "grok-4.6", 10),
            true,
            true,
        );
        let headers = with.headers("grok-4.6", &auth).unwrap();
        let names: Vec<_> = headers.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "content-type",
                "accept",
                "x-grok-conv-id",
                "user-agent",
                "authorization",
                "x-stainless-timeout",
            ]
        );
        assert_eq!(headers["x-grok-conv-id"], "conv");
        assert_eq!(headers["user-agent"], "omp/18.12.6");
        assert_eq!(headers["x-stainless-timeout"], "300");
        assert!(headers["authorization"].is_sensitive());
        assert_eq!(with.prompt_cache_key(), Some("conv"));

        let without = Profile::new(
            XaiKind::XaiOauth,
            &options(None, "grok-4.6", 10),
            true,
            true,
        );
        let headers = without.headers("grok-4.6", &auth).unwrap();
        assert!(!headers.contains_key("x-grok-conv-id"));
        assert_eq!(without.prompt_cache_key(), None);
        assert_eq!(without.replay_provider(), Some("xai-oauth"));
        assert!(without.reject_root_object_union());
        assert_eq!(without.dialect(), Dialect::Generic);
        assert_eq!(without.temperature(), Some(0.5));
    }

    #[test]
    fn blank_bearer_is_an_authentication_failure() {
        let auth = ResolvedAuth::ApiKey(SecretString::new("  ".into()));
        assert!(matches!(
            auth_headers(&auth),
            Err(ProviderError::AuthenticationFailed)
        ));
    }
}
