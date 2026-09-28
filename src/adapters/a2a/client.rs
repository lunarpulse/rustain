//! Hardened AgentCard HTTP client and verified cache.
//!
//! # The three trust inputs (Story 19.14, `AD-1823`)
//!
//! This client now carries two of them. The **TLS anchor** (`caCert`) decides
//! which server it will talk to at all and is applied at builder level, so the
//! handshake precedes every body. The **bearer credential** (`auth`, an
//! environment-variable *name*) is attached per RPC and only to the roster
//! entry's own origin. The third — the AgentCard's Ed25519 producer pin — is
//! unchanged and still the sole source of `TrustTier`.
//!
//! Wire order, which is also the refusal precedence: anchor → card producer →
//! credential. ⛔ The card GET is unauthenticated; ⛔ the credential never rides
//! a builder-level default header.

use std::path::Path;
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderValue, LOCATION};

use crate::domain::models::{A2aPeerSpec, SecretString, TrustTier};

use super::auth::API_KEY_HEADER;
use super::card::{AgentCardView, decode_and_validate};
use super::error::{
    A2aError, AnchorCause, anchor_error, anchor_failure_label, anchor_failure_of, find_rustls_error,
};
use super::jws::{decode_verifying_key, verify_card};

const MAX_REDIRECTS: usize = 5;
const MAX_CARD_BYTES: usize = 1024 * 1024;
const AGENT_CARD_PATH: &str = ".well-known/agent-card.json";

/// What the boot AgentCard fetch left behind (`A22`).
///
/// The old `Option<(AgentCardView, TrustTier)>` fused three different situations
/// into one `None`, and the anchor cause — the only one an operator can act on —
/// was the one that got discarded: the first TLS handshake a peer ever performs
/// is this boot GET, whose error `A2aEgress::compose` only `warn!`s. A send then
/// reported `CardNotCached` and the operator was told discovery might still be in
/// flight when in truth the certificate had been refused.
///
/// ⛔ There is no send-time re-fetch (`A10`): a retained refusal is a boot
/// observation and persists until restart.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CardSlot {
    /// The boot fetch has not finished.
    Pending,
    /// The card was fetched, validated, and (for a pinned peer) JWS-verified.
    Ready(AgentCardView, TrustTier),
    /// The boot fetch failed for a non-anchor reason.
    Unavailable,
    /// The boot fetch — or the anchor load before it — refused this peer's
    /// certificate. Retained so the send surface can name the trust decision.
    AnchorRefused(AnchorCause),
}

/// How one peer's trust store is configured. Derived from `ca_cert`'s presence
/// and nothing else (`A3`); exactly one value per peer, and it drives the step
/// list below — the only place trust-affecting builder settings are decided
/// (`AC3` part 1).
pub(crate) enum TrustMode {
    /// Today's posture: platform roots, today's backend. ⛔ Unchanged.
    PlatformRoots,
    /// Exclusive trust in the operator's anchor, on rustls.
    AnchorOnly(std::path::PathBuf),
}

/// One trust-affecting builder action.
///
/// The applier builds this list first and then touches the builder **only** by
/// iterating it, so a step and its builder call cannot be recorded apart
/// (`AC3` part 1).
enum TrustStep {
    ForceRustls,
    BuiltInRoots(bool),
    AddRoot(reqwest::Certificate),
}

impl TrustStep {
    fn kind(&self) -> TrustStepKind {
        match self {
            Self::ForceRustls => TrustStepKind::ForceRustls,
            Self::BuiltInRoots(enabled) => TrustStepKind::BuiltInRoots(*enabled),
            Self::AddRoot(_) => TrustStepKind::AddRoot,
        }
    }
}

/// The recordable projection of a [`TrustStep`].
///
/// ⚠ `reqwest` exposes no root-store inspection and its `ClientBuilder`'s `Debug`
/// shows the backend but not the root settings, so this list is the **only**
/// evidence that an anchored peer does not also trust the platform roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustStepKind {
    ForceRustls,
    BuiltInRoots(bool),
    AddRoot,
}

/// Turn an anchor path into the step list, or into form 9's cause.
///
/// ⛔ Never a `compose` error: an unloadable anchor must not turn "key not
/// exported yet" or "certificate not copied yet" into a daemon that refuses to
/// boot (`A28` item 1).
fn anchor_steps(path: &Path) -> Result<Vec<TrustStep>, AnchorCause> {
    let bundle = crate::adapters::pem_tls::load_certificate_bundle(path).map_err(|error| {
        AnchorCause::Unloadable {
            reason: error.reason().to_owned(),
        }
    })?;
    let mut steps = Vec::with_capacity(bundle.len() + 2);
    steps.push(TrustStep::ForceRustls);
    steps.push(TrustStep::BuiltInRoots(false));
    for der in &bundle {
        // ⛔ Not `from_pem_bundle`: it answers `Ok(0)` for an empty file and for a
        // key-only PEM, which would install zero anchors and then report every
        // handshake as a mismatch.
        let certificate =
            reqwest::Certificate::from_der(der).map_err(|error| AnchorCause::Unloadable {
                reason: anchor_parse_reason(path, &error),
            })?;
        steps.push(TrustStep::AddRoot(certificate));
    }
    Ok(steps)
}

/// `pem_tls`'s own parse-failure vocabulary, for the two failures that happen
/// inside `reqwest` rather than inside the loader.
fn anchor_parse_reason(path: &Path, error: &reqwest::Error) -> String {
    format!("parsing certificate chain {}: {error}", path.display())
}

fn apply_trust(builder: reqwest::ClientBuilder, steps: Vec<TrustStep>) -> reqwest::ClientBuilder {
    steps.into_iter().fold(builder, |builder, step| match step {
        TrustStep::ForceRustls => builder.use_rustls_tls(),
        TrustStep::BuiltInRoots(enabled) => builder.tls_built_in_root_certs(enabled),
        TrustStep::AddRoot(certificate) => builder.add_root_certificate(certificate),
    })
}

pub struct A2aClientAdapter {
    /// ⛔ No fallback client. A platform-roots client built after an anchor load
    /// failure would make trust silently *additive* — the boot GET could succeed
    /// against a public CA — and a zero-anchor client would overwrite the real
    /// cause with a false mismatch.
    client: Result<reqwest::Client, AnchorCause>,
    base_url_override: Option<String>,
    /// The roster id, for every refusal this client mints.
    alias: String,
    /// The **name** of the variable holding the credential. ⛔ Never its value:
    /// the variable is read per RPC (`A18`), so a key exported after boot works
    /// and a rotated key takes effect on the next call.
    auth_env: Option<String>,
    /// The origin of the configured roster `url`. The credential is sent **only**
    /// here (`A29`) — an AgentCard decides where the RPC goes, so without this a
    /// card or a redirect could name any host with a valid certificate and
    /// collect the operator's key.
    credential_origin: url::Origin,
    /// Whether this peer's client forces rustls, which is the only backend whose
    /// error chain carries a typed `rustls::Error` to classify (`A21`).
    anchored: bool,
    card: tokio::sync::RwLock<CardSlot>,
    #[cfg(any(test, feature = "test-instrumentation"))]
    trust_record: Vec<TrustStepKind>,
    settled: tokio::sync::watch::Sender<bool>,
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
        let base_builder = || {
            let builder = reqwest::Client::builder()
                .https_only(!allow_loopback_http)
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .user_agent(format!("rustain/{}", env!("CARGO_PKG_VERSION")));
            // A loopback-HTTP request is plaintext, and a credentialed one
            // carries `x-api-key` in the clear. reqwest's default builder reads
            // the proxy environment (`HTTP_PROXY`/`ALL_PROXY`) with NO loopback
            // exclusion, which would forward the whole request — credential
            // included — to the proxy host. ⛔ A plaintext loopback peer must
            // never ride a proxy. (https peers are unaffected: a CONNECT tunnel
            // exposes no headers to the proxy.)
            if allow_loopback_http {
                builder.no_proxy()
            } else {
                builder
            }
        };

        #[cfg(any(test, feature = "test-instrumentation"))]
        let mut trust_record: Vec<TrustStepKind> = Vec::new();

        // The ONE trust decision for this peer, derived from `ca_cert`'s
        // presence and nothing else, then driving the step list (`AC3` part 1).
        let trust_mode = match spec.ca_cert.as_deref() {
            None => TrustMode::PlatformRoots,
            Some(path) => TrustMode::AnchorOnly(path.to_owned()),
        };
        let client = match &trust_mode {
            // ⛔ An unpinned peer's builder is untouched: native-tls carries the OS
            // trust store that corporate MITM appliances and platform root policy
            // depend on, and `default-tls` is what this declaration selects today.
            TrustMode::PlatformRoots => Ok(base_builder()
                .build()
                .map_err(|error| A2aError::ClientBuild(error.to_string()))?),
            TrustMode::AnchorOnly(path) => match anchor_steps(path) {
                Err(cause) => Err(cause),
                Ok(steps) => {
                    #[cfg(any(test, feature = "test-instrumentation"))]
                    {
                        trust_record = steps.iter().map(TrustStep::kind).collect();
                    }
                    // `build()` also fails when `RootCertStore::add` rejects a
                    // block, which is a load failure and therefore form 9 —
                    // ⛔ never a `compose` error.
                    apply_trust(base_builder(), steps).build().map_err(|error| {
                        AnchorCause::Unloadable {
                            reason: anchor_parse_reason(path, &error),
                        }
                    })
                }
            },
        };

        let slot = match &client {
            Ok(_) => CardSlot::Pending,
            Err(cause) => CardSlot::AnchorRefused(cause.clone()),
        };
        // ⚠ ALWAYS starts `false`, even when the slot is already
        // `AnchorRefused`: the signal means *"the boot fetch attempt finished"*,
        // not *"the slot is non-Pending"*. Seeding it `true` for an unloadable
        // anchor let a test read the slot BEFORE `compose`'s spawned
        // `refresh_agent_card` ran, so the "a refresh never overwrites
        // AnchorRefused(Unloadable)" invariant was never actually exercised —
        // caught by mutant M22, which stayed GREEN against it.
        let settled = tokio::sync::watch::Sender::new(false);

        Ok(Self {
            client,
            base_url_override,
            alias: spec.id.clone(),
            auth_env: spec.auth.clone(),
            credential_origin: parsed.origin(),
            anchored: spec.ca_cert.is_some(),
            card: tokio::sync::RwLock::new(slot),
            #[cfg(any(test, feature = "test-instrumentation"))]
            trust_record,
            settled,
        })
    }

    pub async fn refresh_agent_card(&self, spec: &A2aPeerSpec) -> Result<(), A2aError> {
        // `A28` item 1: with no client there is nothing to fetch — and nothing to
        // overwrite. ⛔ A refresh never replaces `AnchorRefused(Unloadable)` with a
        // weaker cause; doing so would render `CardNotCached` and tell the
        // operator discovery might still be in flight.
        if let Err(cause) = &self.client {
            self.mark_settled();
            return Err(anchor_error(&self.alias, cause));
        }
        let result = self.fetch_agent_card(spec).await;
        {
            let mut slot = self.card.write().await;
            *slot = match &result {
                Ok((card, trust)) => CardSlot::Ready(card.clone(), *trust),
                Err(error) => slot_for_error(error),
            };
        }
        self.mark_settled();
        result.map(|_| ())
    }

    /// The retained boot outcome.
    ///
    /// ⛔ Consumers must handle `AnchorRefused` before falling back to the
    /// `CardNotCached` vocabulary: that is the whole point of retaining it.
    pub async fn card_slot(&self) -> CardSlot {
        self.card.read().await.clone()
    }

    /// The card, when the boot fetch produced one.
    ///
    /// For every non-`Ready` state this is `None`, which is exactly the shape the
    /// discovery and inventory paths already had (`A28` item 5).
    pub async fn ready_card(&self) -> Option<(AgentCardView, TrustTier)> {
        match &*self.card.read().await {
            CardSlot::Ready(card, trust) => Some((card.clone(), *trust)),
            CardSlot::Pending | CardSlot::Unavailable | CardSlot::AnchorRefused(_) => None,
        }
    }

    /// Await the boot fetch leaving `Pending`.
    ///
    /// A failed boot fetch emits **nothing** — `A2aEgress::compose` sends
    /// `A2aCatalogChanged` only on success — so a test has no event to wait on.
    /// ⛔ Not a sleep and not a poll loop: wrap the call in
    /// `tokio::time::timeout` so a hang becomes a failure.
    pub async fn await_settled(&self) {
        let mut rx = self.settled.subscribe();
        let _ = rx.wait_for(|settled| *settled).await;
    }

    /// The trust steps this peer's client was built from.
    ///
    /// An unpinned peer records **nothing**; an anchored peer records
    /// `ForceRustls`, `BuiltInRoots(false)` and one `AddRoot` per `CERTIFICATE`
    /// block.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(crate) fn trust_steps(&self) -> &[TrustStepKind] {
        &self.trust_record
    }

    fn mark_settled(&self) {
        // `watch::Sender::send` DROPS the value when no receiver exists (it
        // returns `Err` without storing), so a boot refresh that finishes before
        // the first `await_settled()` subscription would be lost and the late
        // subscriber would hang. `send_replace` stores unconditionally.
        self.settled.send_replace(true);
    }

    fn http(&self) -> Result<&reqwest::Client, A2aError> {
        self.client
            .as_ref()
            .map_err(|cause| anchor_error(&self.alias, cause))
    }

    /// POST a JSON-RPC 2.0 request to a resolved A2A endpoint and demux the
    /// response against the request id. This is the transport seam Story 17.4b's
    /// task lifecycle drives through.
    ///
    /// Hard rules (Task 0b spike): **no `A2A-Version` header** (sending one is the
    /// one way to break an agent that would otherwise answer), and the caller MUST
    /// pass the resolved `url` — never the origin, which returns a plain 404. The
    /// https-or-loopback safety policy is enforced here via `parse_and_validate_url`,
    /// so a plain-HTTP non-loopback endpoint (6/141 in the corpus) is refused at
    /// POST time even though endpoint resolution accepted it.
    ///
    /// # Refusal precedence, applied over *observed* causes (`AD-1823`)
    ///
    /// `UnsafeUrl` (unchanged) → the anchor → the credential's origin → the
    /// credential's presence. Every one of those is decided **before a connection
    /// is opened**: ⛔ the client never dials solely to discover a higher-ranked
    /// cause for a send it already knows it cannot complete.
    pub(crate) async fn post_jsonrpc(
        &self,
        endpoint_url: &str,
        request: &super::jsonrpc::JsonRpcRequest,
    ) -> Result<serde_json::Value, A2aError> {
        self.post_jsonrpc_after(endpoint_url, request, || async { Ok(()) })
            .await
    }

    /// POST after `before_send` succeeds. The hook runs only after every
    /// no-I/O refusal and request-build error has been resolved, immediately
    /// before the client can open a connection. A failed append prevents POST.
    pub(crate) async fn post_jsonrpc_after<F, Fut>(
        &self,
        endpoint_url: &str,
        request: &super::jsonrpc::JsonRpcRequest,
        before_send: F,
    ) -> Result<serde_json::Value, A2aError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), A2aError>>,
    {
        let url = parse_and_validate_url(endpoint_url)?;
        let client = self.http()?;
        let mut outbound = client
            .post(url.clone())
            .header(CONTENT_TYPE, "application/json");
        // ⛔ Per request, never `ClientBuilder::default_headers`: a default header
        // would ride the unauthenticated card GET and live for the client's whole
        // lifetime.
        if self.auth_env.is_some() {
            outbound = outbound.header(API_KEY_HEADER, self.credential_header(&url)?);
        }
        let outbound = outbound
            .json(request)
            .build()
            .map_err(|error| self.map_transport_error(&error))?;
        before_send().await?;
        let response = client
            .execute(outbound)
            .await
            .map_err(|error| self.map_transport_error(&error))?;
        if !response.status().is_success() {
            return Err(self.status_error(response.status().as_u16()));
        }
        let body = read_capped_body(response, MAX_CARD_BYTES).await?;
        let raw = std::str::from_utf8(&body).map_err(|_| A2aError::InvalidUtf8)?;
        super::jsonrpc::parse_response(raw, request.id)
    }

    /// The `x-api-key` value for this call, or the refusal that replaces it.
    ///
    /// Read on **every** call (`A18`): the constructor must not read the
    /// environment, because `A2aEgress::compose` propagates its errors and a
    /// variable exported after boot would otherwise be a daemon composition
    /// failure. ⛔ Nothing is cached across calls.
    fn credential_header(&self, endpoint: &url::Url) -> Result<HeaderValue, A2aError> {
        let env_var = self
            .auth_env
            .as_deref()
            .expect("callers check `auth_env` before asking for a credential header");
        // Origin binding first: form 4 outranks form 1, and neither does I/O.
        // Scheme, host and port all count; `Url` normalises case, punycode and
        // default ports, and ⛔ a trailing-dot host is deliberately a different
        // origin.
        if endpoint.origin() != self.credential_origin {
            return Err(A2aError::CredentialOutOfScope {
                alias: self.alias.clone(),
                roster_origin: self.credential_origin.ascii_serialization(),
            });
        }
        let missing = || A2aError::CredentialMissing {
            alias: self.alias.clone(),
            env_var: Some(env_var.to_owned()),
        };
        // An `auth` value that is empty or whitespace is **configured but
        // unusable**: ⛔ never silently treated as "no auth", which would send the
        // request unauthenticated and then name the wrong fix.
        let name = env_var.trim();
        // `std::env::var` PANICS on a name containing `=` or NUL — an operator
        // who pastes `KEY=value` into the auth field hits exactly that, and the
        // panic would kill the spawned delegation task with no typed refusal.
        // An unusable name is "no usable credential": form 1, before any I/O.
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(missing());
        }
        let secret = SecretString::new(
            crate::infrastructure::utils::env_var_trimmed(name).ok_or_else(missing)?,
        );
        // ⛔ The value never appears in the error: a key with a newline or a
        // non-ASCII byte is "no usable credential", not a rendered byte string.
        let mut value = HeaderValue::from_str(secret.expose_secret()).map_err(|_| missing())?;
        // Masks `Debug` and lets caches/HPACK treat the value specially. ⛔ It
        // does not protect memory and must never be described as doing so.
        value.set_sensitive(true);
        Ok(value)
    }

    /// Classify a transport failure, for anchored peers only (`A21`).
    ///
    /// ⛔ No native-tls classifier: an unpinned peer's chain carries an OpenSSL
    /// `ErrorStack`, and labelling a connection failure as an anchor failure is
    /// worse than the bare status.
    fn map_transport_error(&self, error: &reqwest::Error) -> A2aError {
        if self.anchored {
            if let Some(failure) = find_rustls_error(error).and_then(anchor_failure_of) {
                // A fixed discriminant label chosen by `match` — ⛔ never the
                // rustls error's `Debug`/`Display`, which carries the names the
                // peer's certificate presented (`AD-1824`).
                tracing::warn!(
                    peer = %self.alias,
                    anchor_failure = anchor_failure_label(failure),
                    "A2A trust anchor refused this peer's certificate"
                );
                return A2aError::AnchorValidationFailed {
                    alias: self.alias.clone(),
                    failure,
                };
            }
        }
        // A connect failure is PROVEN undelivered: no request byte reached
        // the peer. `Request` cannot say that — it also carries a timeout
        // after the body was sent — so a write verb that must only claim
        // "nothing was marked" when it is true (Story 19.16f `F5`) needs the
        // distinction. The Display is byte-identical to `Request`'s, so
        // `/team send`'s rendered refusal does not change.
        if error.is_connect() {
            return A2aError::Connect(error.to_string());
        }
        A2aError::Request(error.to_string())
    }

    /// An RPC 401/403 is a credential verdict, ⛔ never a bare `HttpStatus`.
    ///
    /// A 401/403 on the **card GET** is unchanged and still `HttpStatus`: that
    /// path leaves the slot `Unavailable`, which renders `CardNotCached`.
    fn status_error(&self, status: u16) -> A2aError {
        if matches!(status, 401 | 403) {
            return match self.auth_env {
                Some(_) => A2aError::CredentialRejected {
                    alias: self.alias.clone(),
                },
                // The bare 401 on an auth-less peer is the exact symptom
                // `DF-18-8-A2A-CLIENT-CREDENTIAL` recorded.
                None => A2aError::CredentialMissing {
                    alias: self.alias.clone(),
                    env_var: None,
                },
            };
        }
        A2aError::HttpStatus { status }
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
                .http()?
                .get(url.clone())
                .send()
                .await
                // The boot card GET is the FIRST TLS handshake a peer ever
                // performs, so this is where an anchor failure is actually
                // observed. Classifying it here is what lets the refusal name the
                // trust decision instead of arriving at `/team send` as
                // `CardNotCached`.
                .map_err(|error| self.map_transport_error(&error))?;
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

/// Which slot a failed boot fetch leaves behind.
///
/// Only the two anchor errors are retained; everything else stays `Unavailable`
/// and keeps `CardNotCached`'s byte-identical text and its pending meaning.
fn slot_for_error(error: &A2aError) -> CardSlot {
    match error {
        A2aError::AnchorValidationFailed { failure, .. } => {
            CardSlot::AnchorRefused(AnchorCause::Validation(*failure))
        }
        A2aError::CaCertUnloadable { reason, .. } => {
            CardSlot::AnchorRefused(AnchorCause::Unloadable {
                reason: reason.clone(),
            })
        }
        _ => CardSlot::Unavailable,
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

pub(crate) fn parse_and_validate_url(raw: &str) -> Result<url::Url, A2aError> {
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

pub(crate) fn is_json_content_type(value: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use crate::domain::models::{A2aPeerSource, A2aPeerSpec, RedactedUrl};

    use super::A2aClientAdapter;

    /// Regression for the code-review settle-signal finding: `watch::Sender::send`
    /// drops the value when no receiver exists, so a settle recorded before any
    /// `await_settled()` subscription must still settle a late subscriber.
    #[tokio::test]
    async fn a_settle_recorded_before_any_subscription_is_not_lost() {
        let spec = A2aPeerSpec::new(
            "settle",
            RedactedUrl::from("http://127.0.0.1:9"),
            A2aPeerSource::Workspace,
        );
        let adapter = A2aClientAdapter::new(&spec, None).expect("adapter composes");

        adapter.mark_settled();

        tokio::time::timeout(std::time::Duration::from_secs(5), adapter.await_settled())
            .await
            .expect("a settle recorded before subscription must still be visible");
    }

    #[tokio::test]
    async fn a_preflight_refusal_does_not_run_the_before_send_hook() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let spec = A2aPeerSpec::new(
            "scoped",
            RedactedUrl::from("http://127.0.0.1:9"),
            A2aPeerSource::Workspace,
        )
        .with_auth(Some("RUSTAIN_UNUSED_TEST_KEY".to_owned()));
        let adapter = A2aClientAdapter::new(&spec, None).expect("adapter composes");
        let request = crate::adapters::a2a::jsonrpc::JsonRpcRequest::new(
            1,
            crate::adapters::a2a::ITEMS_RETRACT_METHOD,
            serde_json::json!({ "itemId": "ri_x" }),
        );
        let hook_ran = AtomicBool::new(false);

        let error = adapter
            .post_jsonrpc_after("http://127.0.0.1:10/a2a", &request, || async {
                hook_ran.store(true, Ordering::Relaxed);
                Ok(())
            })
            .await
            .expect_err("the credential origin is fixed by the roster");

        assert!(matches!(
            error,
            crate::adapters::a2a::error::A2aError::CredentialOutOfScope { .. }
        ));
        assert!(!hook_ran.load(Ordering::Relaxed));
    }
}
