//! OpenRouter request policy for both transports: attribution and cache headers,
//! the Responses body hooks and the Chat Completions profile; adapted from
//! oh-my-pi (`ai/src/utils/openrouter-headers.ts`, `ai/src/providers/
//! openai-responses.ts`, `openai-completions.ts`, `openai-shared.ts`).
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

use super::{Served, errors};
use crate::adapters::openai::{ChatCompat, ChatImages, ChatProfile};
use crate::adapters::provider::http_retry::{HttpRetryPolicy, OPENAI_WIRE};
use crate::adapters::provider::request_profile::{RequestContext, RequestProfile};
use crate::adapters::provider::stream_retry::{self, StreamResilience};
use crate::adapters::responses::{Dialect, WireProfile, transport::Wire};
use crate::domain::{
    errors::ProviderError,
    models::{CompletionOptions, ResolvedAuth},
};
use crate::infrastructure::key_resolver::Resolved;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::sync::Arc;

/// The client identity OpenRouter credits inference traffic to, then omp's
/// categories and response-cache headers. omp sends its own identity here
/// (`User-Agent: omp/<version>`, `HTTP-Referer: https://omp.sh/`,
/// `X-OpenRouter-Title: omp`); app attribution names the client the traffic
/// comes from, so rustain names itself. Nothing functional depends on it.
pub(super) const ATTRIBUTION: &[(&str, &str)] = &[
    ("user-agent", concat!("rustain/", env!("CARGO_PKG_VERSION"))),
    ("http-referer", "https://github.com/lunarpulse/rustain"),
    ("x-openrouter-title", "rustain"),
    ("x-openrouter-categories", "cli-agent"),
    ("x-openrouter-cache", "true"),
    ("x-openrouter-cache-ttl", "3600"),
];

/// The first-event timeout in seconds, announced as `X-Stainless-Timeout`.
const STAINLESS_TIMEOUT: &str = "300";

/// Every inference request's stream `Accept` and the attribution table, for
/// both transports. Never the bearer: each transport adds its own.
pub(super) fn inference_headers(out: &mut HeaderMap) {
    out.insert("accept", HeaderValue::from_static("text/event-stream"));
    for (name, value) in ATTRIBUTION {
        out.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
}

/// omp `facts.is("anthropic")` for OpenRouter ids: the `anthropic/` vendor prefix.
pub(super) fn anthropic_class(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("anthropic/")
}

/// Request-scoped Responses policy; fixed for the logical request, so the body
/// is encoded once.
pub(super) struct Responses {
    cache_key: Option<String>,
    session_id: Option<String>,
    cache_control: Option<Value>,
    reasoning: bool,
    vision: bool,
    temperature: Option<f32>,
}

impl Responses {
    /// `reasoning`/`vision` come from the served catalogue row of `options.model`.
    pub(super) fn new(options: &CompletionOptions, reasoning: bool, vision: bool) -> Self {
        let session = options.session_id.as_deref().filter(|id| !id.is_empty());
        Self {
            cache_key: session.map(crate::adapters::responses::prompt_cache_key),
            session_id: session
                .map(|id| crate::adapters::responses::stable_id(id, 256, "session_")),
            cache_control: anthropic_class(&options.model).then(|| json!({"type": "ephemeral"})),
            reasoning,
            vision,
            temperature: options.temperature,
        }
    }
}

impl WireProfile for Responses {
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
        Some("openrouter")
    }
    fn reasoning(&self) -> bool {
        self.reasoning
    }
    fn vision(&self) -> bool {
        self.vision
    }
    /// `CompletionOptions.max_tokens` is a default, never an explicit cap, and
    /// omp omits defaulted caps on the OpenRouter host.
    fn max_output_tokens(&self) -> Option<u32> {
        None
    }
    fn temperature(&self) -> Option<f32> {
        self.temperature
    }
    fn routing_session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
    fn cache_control(&self) -> Option<&Value> {
        self.cache_control.as_ref()
    }
}

#[async_trait::async_trait]
impl Wire for Responses {
    fn headers(&self, _model: &str, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let bearer = crate::adapters::openai::authorization(auth.bearer_secret())?
            .ok_or(ProviderError::AuthenticationFailed)?;
        headers.insert("authorization", bearer);
        headers.insert(
            "x-stainless-timeout",
            HeaderValue::from_static(STAINLESS_TIMEOUT),
        );
        inference_headers(&mut headers);
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

/// The Chat Completions route under `RUSTAIN_OPENROUTER_RESPONSES=0`.
pub(super) struct OpenRouterChatProfile {
    configured_base: Option<String>,
    served: Arc<Served>,
}

impl OpenRouterChatProfile {
    pub(super) fn new(configured_base: Option<String>, served: Arc<Served>) -> Arc<Self> {
        Arc::new(Self {
            configured_base,
            served,
        })
    }
}

impl std::fmt::Debug for OpenRouterChatProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterChatProfile")
            .field("configured_base", &self.configured_base.is_some())
            .finish_non_exhaustive()
    }
}

impl RequestProfile for OpenRouterChatProfile {
    fn base_url(&self, _resolved: &Resolved) -> Result<String, ProviderError> {
        Ok(super::base_url(self.configured_base.as_deref()))
    }
    fn headers(&self, _resolved: &Resolved, _ctx: &RequestContext<'_>, out: &mut HeaderMap) {
        inference_headers(out);
    }
    fn error(
        &self,
        status: u16,
        headers: &HeaderMap,
        body: &str,
        sent: &HeaderMap,
    ) -> Option<ProviderError> {
        Some(errors::http_error(status, headers, body, sent))
    }
    /// omp `postOpenAIStream` → `fetchWithRetry`: 408/429/5xx and transport
    /// failures at open are resent before the provider loop sees a final answer.
    fn retry(&self) -> Option<&'static HttpRetryPolicy> {
        Some(&OPENAI_WIRE)
    }
    fn resilience(&self) -> Option<&'static StreamResilience> {
        Some(&stream_retry::CHAT)
    }
}

impl ChatProfile for OpenRouterChatProfile {
    /// No `store`, no output cap (a default is never sent), image parts only
    /// for vision models, and an Anthropic cache breakpoint for `anthropic/*`.
    fn compat(&self, _resolved: &Resolved, ctx: &RequestContext<'_>) -> ChatCompat {
        let model = ctx.options.model.as_str();
        let (_, vision) = self.served.traits(model);
        ChatCompat {
            store_false: false,
            max_completion_tokens: None,
            images: if vision {
                ChatImages::Parts
            } else {
                ChatImages::Omitted
            },
            anthropic_cache_control: anthropic_class(model),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(session: Option<&str>, model: &str) -> CompletionOptions {
        CompletionOptions {
            session_id: session.map(str::to_owned),
            model: model.into(),
            max_tokens: 8_192,
            system_prompt: String::new(),
            temperature: None,
            tools: Vec::new(),
        }
    }

    #[test]
    fn the_attribution_table_is_rustains_identity_plus_omps_cache_headers() {
        let mut headers = HeaderMap::new();
        inference_headers(&mut headers);
        let get = |name: &str| headers.get(name).unwrap().to_str().unwrap().to_owned();
        assert_eq!(
            get("user-agent"),
            format!("rustain/{}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(get("http-referer"), "https://github.com/lunarpulse/rustain");
        assert_eq!(get("x-openrouter-title"), "rustain");
        assert_eq!(get("x-openrouter-categories"), "cli-agent");
        assert_eq!(get("x-openrouter-cache"), "true");
        assert_eq!(get("x-openrouter-cache-ttl"), "3600");
        assert_eq!(get("accept"), "text/event-stream");
        assert!(headers.get("authorization").is_none());
    }

    #[test]
    fn session_ids_are_normalised_and_absent_without_a_session() {
        let long = "s".repeat(300);
        let wire = Responses::new(&options(Some(&long), "openai/gpt-5.5"), false, false);
        assert!(wire.prompt_cache_key().unwrap().starts_with("pc_"));
        assert!(wire.routing_session_id().unwrap().starts_with("session_"));
        let wire = Responses::new(&options(Some("abc"), "openai/gpt-5.5"), false, false);
        assert_eq!(wire.prompt_cache_key(), Some("abc"));
        assert_eq!(wire.routing_session_id(), Some("abc"));
        let wire = Responses::new(&options(None, "openai/gpt-5.5"), false, false);
        assert_eq!(wire.prompt_cache_key(), None);
        assert_eq!(wire.routing_session_id(), None);
        assert_eq!(wire.max_output_tokens(), None);
    }

    #[test]
    fn only_anthropic_class_models_get_a_cache_breakpoint() {
        for (model, expected) in [
            ("anthropic/claude-sonnet-4.6", true),
            ("Anthropic/Claude-Opus", true),
            ("openai/gpt-5.5", false),
            ("x/anthropic/claude", false),
        ] {
            let wire = Responses::new(&options(None, model), false, false);
            assert_eq!(wire.cache_control().is_some(), expected, "{model}");
        }
    }
}
