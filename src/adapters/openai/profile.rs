//! Chat Completions request policy: the shared `RequestProfile` plus the
//! request-body compatibility switches of one provider.

use std::sync::Arc;

use reqwest::header::HeaderMap;

use crate::adapters::auth::PROVIDER_TEXT_CAP;
use crate::adapters::provider::request_profile::{RequestContext, RequestProfile};
use crate::domain::errors::ProviderError;
use crate::infrastructure::key_resolver::Resolved;

/// How user images are encoded.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ChatImages {
    /// `[Image: <media_type>]` appended to the text (the historical encoding).
    #[default]
    Legacy,
    /// `image_url` data-URL content parts.
    Parts,
    /// The model has no vision: one placeholder text part replaces the images.
    Omitted,
}

/// Body switches; the default reproduces the historical request bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatCompat {
    /// Send `"store": false`.
    pub store_false: bool,
    /// Send `max_completion_tokens`.
    pub max_completion_tokens: Option<u32>,
    pub images: ChatImages,
    /// Put an ephemeral `cache_control` on the last user/assistant text part
    /// (Anthropic-class models behind a gateway that forwards it).
    pub anthropic_cache_control: bool,
}

/// What a provider sent as an error inside a 2xx answer: a body without any
/// stream event (`in_stream == false`) or an error event inside the stream.
pub struct InBandError<'a> {
    pub status: u16,
    pub content_type: Option<&'a str>,
    /// `error.code`, else `error.type`.
    pub code: Option<&'a str>,
    pub message: Option<&'a str>,
    /// The body prefix or the event data, used when there is no message.
    pub raw: &'a str,
    pub in_stream: bool,
}

/// Longest content type shown, in characters.
const CONTENT_TYPE_CAP: usize = 100;

impl InBandError<'_> {
    /// The text of this error: `prefix` names the provider; `scrub` makes one
    /// COMPLETE provider-supplied value safe (normalised for the terminal, then
    /// the request's credentials removed). Code and message are scrubbed whole,
    /// joined, and only then cut once to `PROVIDER_TEXT_CAP`, so a credential
    /// can neither be rebuilt by normalising nor survive as a cut prefix.
    pub fn text(&self, prefix: &str, scrub: &dyn Fn(&str) -> String) -> String {
        let detail = match (self.code, self.message) {
            (Some(code), Some(message)) => format!("{}: {}", scrub(code), scrub(message)),
            (None, Some(text)) | (Some(text), None) => scrub(text),
            (None, None) if !self.raw.trim().is_empty() => scrub(self.raw),
            (None, None) if self.in_stream => "no details".to_owned(),
            (None, None) => "empty body".to_owned(),
        };
        let detail: String = detail.chars().take(PROVIDER_TEXT_CAP).collect();
        if self.in_stream {
            format!("{prefix} stream error {detail}")
        } else {
            let kind = self.content_type.map_or_else(
                || "no content-type".to_owned(),
                |kind| scrub(kind).chars().take(CONTENT_TYPE_CAP).collect(),
            );
            format!(
                "{prefix} answered HTTP {} ({kind}) without a stream event: {detail}",
                self.status
            )
        }
    }
}

/// Removes the sent bearer from provider-supplied text.
pub fn remove_bearer(text: &str, sent: &HeaderMap) -> String {
    match sent
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
        .filter(|v| !v.is_empty())
    {
        Some(bearer) => text.replace(bearer, "[redacted]"),
        None => text.to_owned(),
    }
}

pub trait ChatProfile: RequestProfile {
    /// Body switches for the credential `resolved` this request sends.
    fn compat(&self, _resolved: &Resolved, _ctx: &RequestContext<'_>) -> ChatCompat {
        ChatCompat::default()
    }

    /// One complete provider-supplied value made safe to show or log: normalised
    /// first (terminal controls, invisible characters), then `sent`'s
    /// credentials removed. Cap the result afterwards, never before.
    fn scrub(&self, text: &str, sent: &HeaderMap) -> String {
        remove_bearer(
            &crate::adapters::github_copilot::plan::display_safe(text),
            sent,
        )
    }

    /// The text of an error the provider sent inside a 2xx answer; `sent` are
    /// the request headers and `model` the id the request sent.
    fn in_band_error(&self, error: &InBandError<'_>, sent: &HeaderMap, _model: &str) -> String {
        error.text("The provider", &|part| self.scrub(part, sent))
    }
}

/// A profile bound to the request whose answer it words errors for; built once
/// per attempt and shared by the decoder.
pub struct InBandText {
    profile: Arc<dyn ChatProfile>,
    sent: HeaderMap,
    model: String,
    status: u16,
}

impl InBandText {
    pub fn new(profile: Arc<dyn ChatProfile>, sent: HeaderMap, model: String, status: u16) -> Self {
        Self {
            profile,
            sent,
            model,
            status,
        }
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn text(&self, error: &InBandError<'_>) -> String {
        self.profile.in_band_error(error, &self.sent, &self.model)
    }

    /// `ChatProfile::scrub` for this request.
    pub fn scrub(&self, text: &str) -> String {
        self.profile.scrub(text, &self.sent)
    }
}

/// A fixed, normalized base URL with today's headers and body.
#[derive(Debug)]
pub struct StaticChatProfile {
    base_url: String,
}

impl StaticChatProfile {
    pub fn new(base_url: &str) -> Arc<Self> {
        Arc::new(Self {
            base_url: crate::infrastructure::utils::normalize_base_url(base_url),
        })
    }
}

impl RequestProfile for StaticChatProfile {
    fn base_url(&self, _resolved: &Resolved) -> Result<String, ProviderError> {
        Ok(self.base_url.clone())
    }
}

impl ChatProfile for StaticChatProfile {}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn sent() -> HeaderMap {
        let mut sent = HeaderMap::new();
        sent.insert(
            "authorization",
            HeaderValue::from_static("Bearer SECRETTOKEN12345"),
        );
        sent
    }

    fn error<'a>(message: Option<&'a str>, raw: &'a str, in_stream: bool) -> InBandError<'a> {
        InBandError {
            status: 200,
            content_type: Some("application/json"),
            code: Some("zz_code"),
            message,
            raw,
            in_stream,
        }
    }

    #[test]
    fn the_default_text_removes_the_bearer_and_bounds_each_part() {
        let profile = StaticChatProfile::new("http://stub.invalid/v1");
        let hostile = format!("a\u{1b}[2J\u{7} SECRETTOKEN12345 {}", "x".repeat(2000));
        for in_stream in [false, true] {
            let text = profile.in_band_error(&error(Some(&hostile), "", in_stream), &sent(), "m");
            assert!(!text.contains("SECRETTOKEN12345"), "{text}");
            assert!(!text.chars().any(char::is_control), "{text:?}");
            assert!(text.contains("zz_code"), "{text}");
            assert!(text.chars().count() < 700, "{}", text.chars().count());
        }
    }

    #[test]
    fn the_default_text_says_where_the_words_came_from() {
        let profile = StaticChatProfile::new("http://stub.invalid/v1");
        let whole = InBandError {
            code: None,
            ..error(None, "  ", false)
        };
        let text = profile.in_band_error(&whole, &sent(), "m");
        assert!(
            text.contains("HTTP 200 (application/json)") && text.contains("empty body"),
            "{text}"
        );
        let raw = InBandError {
            code: None,
            content_type: None,
            ..error(None, "<html>nope</html>", false)
        };
        let text = profile.in_band_error(&raw, &sent(), "m");
        assert!(
            text.contains("no content-type") && text.contains("<html>nope</html>"),
            "{text}"
        );
        let neutral =
            profile.in_band_error(&error(Some("request rejected"), "", true), &sent(), "m");
        assert!(
            !crate::adapters::responses::events::generic_retryable(&neutral),
            "{neutral}"
        );
    }
}
