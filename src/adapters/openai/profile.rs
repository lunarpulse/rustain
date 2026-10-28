//! Chat Completions request policy: the shared `RequestProfile` plus the
//! request-body compatibility switches of one provider.

use std::sync::Arc;

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
}

pub trait ChatProfile: RequestProfile {
    fn compat(&self, _ctx: &RequestContext<'_>) -> ChatCompat {
        ChatCompat::default()
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
