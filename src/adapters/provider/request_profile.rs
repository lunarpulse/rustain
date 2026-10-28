//! Provider request policy shared by the Chat Completions, Messages and
//! Responses transports: base URL, bearer, headers, per-request identity state,
//! error mapping, HTTP attempts and stream resilience.

use std::{borrow::Cow, fmt};

use reqwest::header::HeaderMap;

use crate::{
    domain::{
        errors::ProviderError,
        models::{CompletionOptions, Message, SecretString},
    },
    infrastructure::key_resolver::Resolved,
};

use super::{http_retry::HttpRetryPolicy, stream_retry::StreamResilience};

/// The logical request a profile shapes headers for.
pub struct RequestContext<'a> {
    pub messages: &'a [Message],
    pub options: &'a CompletionOptions,
}

/// One header-only resend of a denied attempt. `settle` receives whether the
/// resend's answer was a 2xx; a resend that gets no answer (transport error,
/// deadline, cancellation) is never settled, as in omp.
pub struct Alternate {
    pub headers: HeaderMap,
    pub settle: Box<dyn FnOnce(bool) + Send>,
}

/// Per-logical-request state created by `RequestProfile::begin`; owned by the
/// transport for that request only.
pub trait RequestState: Send {
    /// Request-scoped headers, applied after `RequestProfile::headers` on every
    /// HTTP attempt (they override).
    fn headers(&self, _out: &mut HeaderMap) {}
    /// At most one alternate per HTTP attempt, decided only from this state
    /// (never from shared mutable profile state). `sent` holds the headers of
    /// the denied attempt.
    fn alternate(&mut self, _status: u16, _body: &[u8], _sent: &HeaderMap) -> Option<Alternate> {
        None
    }
}

/// Request policy of one provider. One `Arc<dyn RequestProfile>` serves every
/// route and concurrent request, so it never holds per-request mutable state.
pub trait RequestProfile: Send + Sync + fmt::Debug {
    fn base_url(&self, resolved: &Resolved) -> Result<String, ProviderError>;
    fn bearer<'r>(&self, resolved: &'r Resolved) -> Result<Cow<'r, SecretString>, ProviderError> {
        Ok(Cow::Borrowed(resolved.auth.bearer_secret()))
    }
    fn headers(&self, _resolved: &Resolved, _ctx: &RequestContext<'_>, _out: &mut HeaderMap) {}
    /// Called once per logical request after credential resolution, and again
    /// after a 401 recovery replaces the `Resolved`. `None` (the default) means
    /// no request-scoped headers and no alternate.
    fn begin(
        &self,
        _resolved: &Resolved,
        _ctx: &RequestContext<'_>,
    ) -> Option<Box<dyn RequestState>> {
        None
    }
    /// Provider-branded mapping of a final non-2xx response (`sent` holds the
    /// request headers, for redaction); `None` keeps the transport's mapping.
    fn error(
        &self,
        _status: u16,
        _headers: &HeaderMap,
        _body: &str,
        _sent: &HeaderMap,
    ) -> Option<ProviderError> {
        None
    }
    /// HTTP attempts; `None` = one attempt.
    fn retry(&self) -> Option<&'static HttpRetryPolicy> {
        None
    }
    /// Replay-safe stream re-issue and watchdogs; `None` = today's single stream.
    fn resilience(&self) -> Option<&'static StreamResilience> {
        None
    }
}
