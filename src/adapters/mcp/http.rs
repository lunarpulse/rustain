//! Streamable HTTP client transport wiring for MCP (Story 9.9).
//!
//! Three things live here, and they are here rather than in `client.rs`
//! because each is a decision the connect path only *consumes*:
//!
//! 1. [`url_is_loopback`] — **THE** loopback predicate. Ruling A7 tiers
//!    `rustain doctor` on *whose box the server is*, and ruling D2 warns once
//!    on a non-loopback plaintext URL. ⛔ One computation, two consumers: two
//!    independent loopback checks that can disagree is the defect A7 spent its
//!    precision to avoid, so the answer is computed here, once, and travels
//!    with the error (`McpError::Http { local, .. }`) to the doctor.
//! 2. [`transport_config`] — the rmcp transport config, pinned to
//!    [`NeverRetry`] so the SDK's default `ExponentialBackoff` never stacks a
//!    second retry loop underneath rustain's own 5-attempt backoff (AC4).
//! 3. [`classify_init_error`] — the three distinct, actionable HTTP failure
//!    classes AC6 requires, derived from rmcp's real typed errors.
//!
//! ⚠ Naming rmcp's HTTP error types requires naming rmcp's `reqwest`, which is
//! 0.13 — a *different crate* from rustain's own 0.12 at `Cargo.toml:77`. The
//! `reqwest13` alias in `Cargo.toml` binds rmcp's already-present 0.13 under a
//! second name for exactly this reason and adds zero crates to any feature set.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use futures::{StreamExt as _, stream::BoxStream};
use reqwest13::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use rmcp::model::{ClientJsonRpcMessage, JsonRpcMessage, ServerJsonRpcMessage};
use rmcp::service::{ClientInitializeError, RoleClient, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::common::client_side_sse::NeverRetry;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, StreamableHttpClient,
    StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::{Error as SseError, Sse, SseStream};

use crate::domain::models::McpServerSpec;

use super::error::{HttpFailureKind, McpError};

/// HTTP client that preserves the raw JSON response long enough to wrap any
/// task-shaped result before rmcp's untagged `ServerResult` can discard fields.
#[derive(Clone, Default)]
pub(crate) struct GuardedHttpClient(reqwest13::Client);

/// The rmcp transport rustain actually builds. Named once so the error
/// downcast in [`classify_init_error`] and the construction site cannot drift
/// apart — `DynamicTransportError::downcast` compares `TypeId::of::<T>()`, so a
/// mismatch here would silently degrade every failure to `ServerError`.
type HttpTransport = StreamableHttpClientTransport<GuardedHttpClient>;

impl StreamableHttpClient for GuardedHttpClient {
    type Error = reqwest13::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        const JSON: &str = "application/json";
        const EVENTS: &str = "text/event-stream";
        const SESSION: &str = "mcp-session-id";

        let mut request = self
            .0
            .post(uri.as_ref())
            .header(ACCEPT, [EVENTS, JSON].join(", "));
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(SESSION, session_id.as_ref());
        }
        for (name, value) in custom_headers {
            request = request.header(name, value);
        }

        let response = request.json(&message).send().await?;
        let status = response.status();
        if status == reqwest13::StatusCode::UNAUTHORIZED
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        {
            let challenge = header
                .to_str()
                .map_err(|_| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                        "invalid www-authenticate header value",
                    ))
                })?
                .to_string();
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                challenge,
            )));
        }
        if status == reqwest13::StatusCode::FORBIDDEN
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        {
            let challenge = header
                .to_str()
                .map_err(|_| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                        "invalid www-authenticate header value",
                    ))
                })?
                .to_string();
            return Err(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(challenge, None),
            ));
        }
        if matches!(
            status,
            reqwest13::StatusCode::ACCEPTED | reqwest13::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == reqwest13::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).to_string());
        let response_session = response
            .headers()
            .get(SESSION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read response body>".to_string());
            if content_type
                .as_deref()
                .is_some_and(|value| value.as_bytes().starts_with(JSON.as_bytes()))
                && let Ok(message @ JsonRpcMessage::Error(_)) = decode_guarded_message(&body)
            {
                return Ok(StreamableHttpPostResponse::Json(message, response_session));
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {body}"),
            )));
        }

        match content_type.as_deref() {
            Some(value) if value.as_bytes().starts_with(EVENTS.as_bytes()) => {
                let stream = SseStream::from_byte_stream(response.bytes_stream()).boxed();
                Ok(StreamableHttpPostResponse::Sse(stream, response_session))
            }
            Some(value) if value.as_bytes().starts_with(JSON.as_bytes()) => {
                let body = response.text().await?;
                match decode_guarded_message(&body) {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(message, response_session)),
                    Err(error) => {
                        tracing::warn!(
                            "could not parse JSON response as ServerJsonRpcMessage, treating as accepted: {error}"
                        );
                        Ok(StreamableHttpPostResponse::Accepted)
                    }
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        <reqwest13::Client as StreamableHttpClient>::delete_session(
            &self.0,
            uri,
            session_id,
            auth_token,
            custom_headers,
        )
        .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        <reqwest13::Client as StreamableHttpClient>::get_stream(
            &self.0,
            uri,
            session_id,
            last_event_id,
            auth_token,
            custom_headers,
        )
        .await
    }
}

fn decode_guarded_message(body: &str) -> Result<ServerJsonRpcMessage, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(body)?;
    if let Some(result) = value.get_mut("result")
        && super::tasks::is_task_shaped_result(result)
    {
        let raw = std::mem::take(result);
        *result = super::tasks::wrap_task_result(raw);
    }
    serde_json::from_value(value)
}

/// Process-wide fallback for the static auth token (scope cut ①: an env var,
/// not OAuth). The token is the bare credential — rmcp prepends `Bearer `.
pub const AUTH_TOKEN_ENV: &str = "RUSTAIN_MCP_HTTP_AUTH_TOKEN";

/// Per-server override: `RUSTAIN_MCP_HTTP_AUTH_TOKEN_<ID>`, where `<ID>` is the
/// server id upper-cased with every non-alphanumeric byte replaced by `_`.
pub fn auth_token_env_for(server_id: &str) -> String {
    let mut name = String::with_capacity(AUTH_TOKEN_ENV.len() + 1 + server_id.len());
    name.push_str(AUTH_TOKEN_ENV);
    name.push('_');
    for ch in server_id.chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch.to_ascii_uppercase());
        } else {
            name.push('_');
        }
    }
    name
}

fn auth_token(server_id: &str) -> Option<String> {
    // The shared `env_var_trimmed` wrapper lives in `infrastructure/`, which
    // `adapters/mcp` production code is forbidden to import (17.5a R-4,
    // enforced by `mcp_adapter_production_code_never_imports_infrastructure`).
    // Same shape and same exception as
    // `domain/models/mcp_server_spec.rs::expand_env_vars`.
    let read = |key: &str| std::env::var(key).ok().map(|v| v.trim().to_owned()); // CONFORMANCE_EXCEPTION: adapters/mcp cannot import the infrastructure wrapper
    read(&auth_token_env_for(server_id))
        .into_iter()
        .chain(read(AUTH_TOKEN_ENV))
        .find(|token| !token.is_empty())
}

/// True when `url`'s host is loopback — `127.0.0.0/8`, `::1`, or the literal
/// hostname `localhost`.
///
/// DNS is deliberately not consulted, matching `a2a/auth.rs`'s bind predicate:
/// a name that resolves to loopback today may not tomorrow, and a warning that
/// flickers with the resolver is worse than no warning. An unparseable URL is
/// **not** loopback — it never reaches the config gate's happy path anyway.
pub fn url_is_loopback(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .is_some_and(|parsed| parsed_url_is_loopback(&parsed))
}

/// Loopback predicate for an already-validated URL.
///
/// Production parses each endpoint once in `McpClientAdapter::new` and carries
/// this verdict through warnings, connect failures, and doctor timeouts.
pub fn parsed_url_is_loopback(parsed: &url::Url) -> bool {
    match parsed.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

/// True when `url` is a plaintext `http://` URL pointed at a non-loopback host
/// — the exact condition ruling D2 warns about, once, and then allows.
pub fn warrants_plaintext_notice(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "http" && !url_is_loopback(url)
}

/// Build the rmcp transport config for `spec`.
///
/// ⛔ `retry_config` has **no builder** — the field is assigned directly, which
/// is legal cross-crate because `StreamableHttpClientTransportConfig` is
/// `#[non_exhaustive]` but every field is `pub`.
pub fn transport_config(spec: &McpServerSpec, url: &str) -> StreamableHttpClientTransportConfig {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_owned());
    // AC4: rustain owns the retry loop (5 attempts, 1/2/4/8/16s). The SDK
    // default is `ExponentialBackoff`, which would reconnect the standalone SSE
    // response stream underneath us — a second, invisible retry owner.
    // `NeverRetry` is `#[non_exhaustive]`, so `Default` is the only constructor.
    config.retry_config = Arc::new(NeverRetry::default());
    if let Some(token) = auth_token(&spec.id) {
        config.auth_header = Some(token);
    }
    config
}

/// Construct the transport. Requires a live Tokio runtime: `WorkerTransport::spawn`
/// calls `tokio::spawn` immediately.
pub(crate) fn build_transport(config: StreamableHttpClientTransportConfig) -> HttpTransport {
    StreamableHttpClientTransport::with_client(GuardedHttpClient::default(), config)
}

/// Map an rmcp initialize failure onto one of AC6's distinct classes.
///
/// `local` is the loopback answer computed ONCE by [`url_is_loopback`] and
/// carried into the error so `rustain doctor` can tier on it (A7).
pub fn classify_init_error(error: ClientInitializeError, local: bool) -> McpError {
    match error {
        ClientInitializeError::Cancelled => McpError::Cancelled,
        ClientInitializeError::TransportError { error, context } => {
            match error.downcast::<HttpTransport, RoleClient>() {
                Ok(transport_error) => classify_transport_error(transport_error, local),
                // Unreachable while `HttpTransport` names the transport we
                // build; answered rather than panicked because a reachable
                // panic on the connect path would take the app down.
                Err(other) => McpError::Http {
                    kind: HttpFailureKind::ServerError,
                    local,
                    detail: format!("{other} (while {context})"),
                },
            }
        }
        // The initialize POST was accepted and the exchange failed afterwards:
        // a non-response, a JSON-RPC error, a mismatched id, a closed worker.
        // All of these mean the server answered and answered wrongly.
        other => McpError::Http {
            kind: HttpFailureKind::ServerError,
            local,
            detail: format!("{other}"),
        },
    }
}

/// Map a post-initialize request failure onto the same HTTP vocabulary used
/// during initialization. `tools/list` is part of `connect()`; allowing it to
/// collapse back to `ToolsListFailed` would hide 401/5xx remediation.
pub fn classify_service_error(error: ServiceError, local: bool) -> McpError {
    match error {
        ServiceError::TransportSend(error) => match error.downcast::<HttpTransport, RoleClient>() {
            Ok(transport_error) => classify_transport_error(transport_error, local),
            Err(other) => McpError::Http {
                kind: HttpFailureKind::ServerError,
                local,
                detail: other.to_string(),
            },
        },
        ServiceError::TransportClosed | ServiceError::Timeout { .. } => McpError::Http {
            kind: HttpFailureKind::Unreachable,
            local,
            detail: error.to_string(),
        },
        ServiceError::Cancelled { .. } => McpError::Cancelled,
        other => McpError::Http {
            kind: HttpFailureKind::ServerError,
            local,
            detail: other.to_string(),
        },
    }
}

fn classify_transport_error(error: StreamableHttpError<reqwest13::Error>, local: bool) -> McpError {
    let (kind, detail) = match error {
        StreamableHttpError::AuthRequired(auth) => (
            HttpFailureKind::AuthRequired,
            format!(
                "server demands authentication — WWW-Authenticate: {}",
                auth.www_authenticate_header
            ),
        ),
        StreamableHttpError::InsufficientScope(scope) => (
            HttpFailureKind::AuthRequired,
            format!(
                "credential lacks the required scope — WWW-Authenticate: {}",
                scope.www_authenticate_header
            ),
        ),
        StreamableHttpError::Client(client_error) => {
            // rmcp has already split HTTP status/protocol failures into the
            // typed arms above/below. A remaining reqwest client error happened
            // before a usable response (connect, TLS, reset, or timeout).
            let kind = if looks_like_dns_failure(&client_error) {
                HttpFailureKind::DnsFailure
            } else {
                HttpFailureKind::Unreachable
            };
            (kind, format!("{client_error}"))
        }
        other => (HttpFailureKind::ServerError, format!("{other}")),
    };
    McpError::Http {
        kind,
        local,
        detail,
    }
}

/// Resolver failures reach us as an opaque connect error: `reqwest` has no
/// `is_dns()`, and neither `StreamableHttpError` nor `reqwest::Error` exposes
/// the resolver verdict as a variant. The only signal is the message the
/// platform resolver produced, so match on that — and fall back to
/// `Unreachable`, never to a guess that names the operator's config as broken.
fn looks_like_dns_failure(error: &(dyn std::error::Error + 'static)) -> bool {
    const MARKERS: &[&str] = &[
        "dns error",
        "failed to lookup address information",
        "name or service not known",
        "nodename nor servname provided",
        "no such host",
        "temporary failure in name resolution",
    ];
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(err) = current {
        let text = err.to_string().to_ascii_lowercase();
        if MARKERS.iter().any(|marker| text.contains(marker)) {
            return true;
        }
        current = err.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_recognised_and_others_are_not() {
        for url in [
            "http://127.0.0.1:13001/mcp",
            "http://127.7.7.7:13001/mcp",
            "http://[::1]:13001/mcp",
            "http://localhost:13001/mcp",
            "http://LOCALHOST:13001/mcp",
            "https://localhost/mcp",
        ] {
            assert!(url_is_loopback(url), "{url} must be loopback");
        }
        for url in [
            "http://0.0.0.0:13001/mcp",
            "http://192.168.1.10:13001/mcp",
            "http://mcp.example.com/mcp",
            "not a url at all",
        ] {
            assert!(!url_is_loopback(url), "{url} must NOT be loopback");
        }
    }

    #[test]
    fn only_plaintext_non_loopback_urls_warrant_the_d2_notice() {
        assert!(warrants_plaintext_notice("http://mcp.example.com/mcp"));
        assert!(warrants_plaintext_notice("http://0.0.0.0:13001/mcp"));
        // Loopback is silent, and https is silent regardless of host.
        assert!(!warrants_plaintext_notice("http://127.0.0.1:13001/mcp"));
        assert!(!warrants_plaintext_notice("http://localhost:13001/mcp"));
        assert!(!warrants_plaintext_notice("https://mcp.example.com/mcp"));
    }

    #[test]
    fn per_server_auth_env_name_is_sanitised() {
        assert_eq!(
            auth_token_env_for("remote-ci"),
            "RUSTAIN_MCP_HTTP_AUTH_TOKEN_REMOTE_CI"
        );
        assert_eq!(
            auth_token_env_for("Acme.Tools"),
            "RUSTAIN_MCP_HTTP_AUTH_TOKEN_ACME_TOOLS"
        );
    }

    #[test]
    fn post_initialize_service_errors_keep_http_vocabulary() {
        let timeout = classify_service_error(
            ServiceError::Timeout {
                timeout: std::time::Duration::from_secs(1),
            },
            true,
        );
        assert!(matches!(
            timeout,
            McpError::Http {
                kind: HttpFailureKind::Unreachable,
                local: true,
                ..
            }
        ));

        let cancelled = classify_service_error(ServiceError::Cancelled { reason: None }, false);
        assert!(matches!(cancelled, McpError::Cancelled));

        let protocol = classify_service_error(ServiceError::UnexpectedResponse, false);
        assert!(matches!(
            protocol,
            McpError::Http {
                kind: HttpFailureKind::ServerError,
                local: false,
                ..
            }
        ));
    }
    /// The end-to-end DNS path cannot be exercised without a resolver query,
    /// and no test in this repo may reach the network — so the *predicate* is
    /// tested against the messages real platform resolvers produce, nested one
    /// level down exactly as `reqwest` nests them.
    #[test]
    fn dns_marker_detection_walks_the_source_chain() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error sending request")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        for message in [
            "dns error: failed to lookup address information: Name or service not known",
            "Temporary failure in name resolution",
            "nodename nor servname provided, or not known",
        ] {
            let error = Wrapper(std::io::Error::other(message));
            assert!(
                looks_like_dns_failure(&error),
                "{message:?} must classify as a DNS failure"
            );
        }

        let refused = Wrapper(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(
            !looks_like_dns_failure(&refused),
            "a refused connection is unreachable, not a DNS failure"
        );
    }
}
