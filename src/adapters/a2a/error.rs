//! Typed A2A failures.
//!
//! # The refusal contract (Story 19.14, `FR166` / `UX-DR-TM-10` v0.6)
//!
//! `AD-1823` names three independent trust inputs — the server's TLS anchor, the
//! AgentCard's producer pin, and the client's bearer credential — and requires
//! that a refusal name **exactly one** of them. The nine rendered forms below are
//! the complete operator-facing set this module mints; each `#[error]` text **is**
//! the rendered string, so the render boundary matches on the variant and ⛔ never
//! prefixes or string-matches a `Display`.
//!
//! ⛔ No form may carry the secret (value, prefix, length or hash), a bare
//! transport status, server-supplied text, or any name a peer's certificate
//! presents (`AD-1824`).

/// Which anchor validation failed, for the four sub-causes `UX-DR-TM-10` v0.6
/// separates.
///
/// ⚠ The distinction is not cosmetic: *"does not match the pinned anchor"* sends
/// an operator to re-pin a file, which is the wrong fix for an expiry, a wrong
/// hostname, or a CA served as a leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnchorFailure {
    /// Unknown issuer, bad signature, or any other validation failure.
    Mismatch,
    /// Expired or not yet valid.
    OutsideValidity,
    /// Not valid for the roster address.
    WrongName,
    /// The server presented a CA certificate as its own end-entity certificate.
    CaAsServerCert,
}

/// Why an anchored peer is refused: the handshake rejected the certificate, or
/// the configured anchor could not be loaded at all.
///
/// `Clone` (unlike [`A2aError`], which holds a `serde_json::Error`) because the
/// card slot retains it for the lifetime of the process (`A22`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnchorCause {
    Validation(AnchorFailure),
    Unloadable { reason: String },
}

/// The one formatter for forms 5–9.
///
/// Used by both [`A2aError`]'s anchor variants and `SendError::AnchorRefused`'s
/// `Display`, because two copies of a ratified string are two strings waiting to
/// diverge.
pub fn anchor_refusal(alias: &str, cause: &AnchorCause) -> String {
    match cause {
        AnchorCause::Validation(failure) => validation_refusal(alias, *failure),
        AnchorCause::Unloadable { reason } => unloadable_refusal(alias, reason),
    }
}

/// The [`A2aError`] a retained [`AnchorCause`] maps to.
///
/// The card slot keeps the *cause*; the journal row and the `Display` want an
/// error. One conversion so the two cannot drift.
pub fn anchor_error(alias: &str, cause: &AnchorCause) -> A2aError {
    match cause {
        AnchorCause::Validation(failure) => A2aError::AnchorValidationFailed {
            alias: alias.to_owned(),
            failure: *failure,
        },
        AnchorCause::Unloadable { reason } => A2aError::CaCertUnloadable {
            alias: alias.to_owned(),
            reason: reason.clone(),
        },
    }
}

fn validation_refusal(alias: &str, failure: AnchorFailure) -> String {
    match failure {
        AnchorFailure::Mismatch => {
            format!("{alias}'s certificate does not match the pinned anchor")
        }
        AnchorFailure::OutsideValidity => {
            format!("{alias}'s certificate has expired or is not yet valid")
        }
        AnchorFailure::WrongName => {
            format!("{alias}'s certificate is not valid for its roster address")
        }
        AnchorFailure::CaAsServerCert => {
            format!("{alias} presents a CA certificate as its server certificate")
        }
    }
}

fn unloadable_refusal(alias: &str, reason: &str) -> String {
    format!("{alias}'s pinned anchor could not be loaded: {reason}")
}

/// Forms 1 and 2, selected by whether an `auth` field exists at all.
///
/// ⛔ Selected on the `Option`, never on a string: for a peer with **no** `auth`
/// field, form 1 would tell the operator to set a variable named in a field that
/// does not exist.
fn credential_missing(alias: &str, env_var: Option<&String>) -> String {
    match env_var {
        Some(_) => format!("no credential for {alias}: set the env var named in its auth field"),
        None => format!(
            "no credential configured for {alias}: add an auth field naming the env var that \
             holds its key"
        ),
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aError {
    #[error("invalid AgentCard JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("malformed AgentCard: required field {field} is missing or empty")]
    MalformedCard { field: String },
    #[error("AgentCard has no signatures")]
    MissingSignatures,
    #[error("invalid JWS protected header")]
    InvalidProtectedHeader,
    #[error("unsupported AgentCard signature algorithm {algorithm:?}")]
    UnsupportedAlgorithm { algorithm: String },
    #[error("AgentCard signature key id does not match configured pin")]
    KeyIdMismatch {
        expected: String,
        actual: Option<String>,
    },
    #[error("invalid pinned Ed25519 public key")]
    InvalidPinnedKey,
    #[error("invalid AgentCard signature encoding")]
    InvalidSignatureEncoding,
    #[error("AgentCard JCS canonicalization failed: {0}")]
    Canonicalization(String),
    #[error("AgentCard signature verification failed")]
    BadSignature,
    #[error("failed to build A2A HTTP client: {0}")]
    ClientBuild(String),
    #[error("unsafe A2A URL: {reason}")]
    UnsafeUrl { reason: String },
    #[error("A2A request failed: {0}")]
    Request(String),
    #[error("A2A peer returned HTTP {status}")]
    HttpStatus { status: u16 },
    #[error("A2A redirect is invalid: {0}")]
    InvalidRedirect(String),
    #[error("A2A redirect limit exceeded")]
    TooManyRedirects,
    #[error("AgentCard response has non-JSON content type {content_type:?}")]
    UnexpectedContentType { content_type: Option<String> },
    #[error("AgentCard response exceeds {max_bytes} byte limit")]
    BodyTooLarge { max_bytes: usize },
    #[error("AgentCard response is not UTF-8")]
    InvalidUtf8,
    #[error("A2A card exposes no reachable JSON-RPC endpoint: {reason}")]
    NoJsonRpcEndpoint { reason: String },
    #[error(
        "unrecognized A2A task-state spelling {raw:?} — the JSON-RPC binding is \
         lowercase-hyphen; refuse never mis-parse"
    )]
    UnknownTaskState { raw: String },
    #[error("A2A JSON-RPC error {code}: {message}")]
    JsonRpc { code: i64, message: String },
    #[error("malformed A2A JSON-RPC response: {reason}")]
    MalformedResponse { reason: String },
    #[error("A2A JSON-RPC response id {actual:?} does not correlate with request id {expected}")]
    CorrelationMismatch { expected: u64, actual: String },
    #[error("A2A server configuration is invalid: {0}")]
    Config(String),

    // ── Story 19.14: the credential and anchor refusals (forms 1–9) ──────────
    /// Forms 1 (`env_var: Some`) and 2 (`env_var: None`). ⛔ `env_var` holds the
    /// variable's **name** and is never rendered — it only selects the form.
    #[error("{}", credential_missing(.alias, .env_var.as_ref()))]
    CredentialMissing {
        alias: String,
        env_var: Option<String>,
    },
    /// Form 3. ⛔ No status code and no server-supplied text: the client cannot
    /// tell a wrong secret here from a revoked grant there and must not imply it
    /// can.
    #[error("{alias} rejected this credential")]
    CredentialRejected { alias: String },
    /// Form 4. `roster_origin` is the operator's own configured origin — ⛔ never
    /// the card's endpoint, which is peer-controlled text.
    #[error(
        "{alias}'s card sends requests to another host; its credential is only sent to \
         {roster_origin}"
    )]
    CredentialOutOfScope {
        alias: String,
        roster_origin: String,
    },
    /// Forms 5–8. ⛔ No certificate bytes, subject, issuer or rustls text.
    #[error("{}", anchor_refusal(.alias, &AnchorCause::Validation(*.failure)))]
    AnchorValidationFailed {
        alias: String,
        failure: AnchorFailure,
    },
    /// Form 9. `reason` is `pem_tls`'s own vocabulary verbatim (it names the path
    /// and the fault).
    #[error("{}", unloadable_refusal(.alias, .reason))]
    CaCertUnloadable { alias: String, reason: String },
}

/// Find the `rustls::Error` a `reqwest` transport failure is really carrying.
///
/// # Why a plain `source()` walk returns nothing (`A21`, measured at
/// `reqwest` 0.12.28 / `tokio-rustls` 0.26.4 / `hyper-rustls` 0.27.7)
///
/// A certificate failure arrives as a pre-response **connect** error, and the
/// chain is
///
/// ```text
/// reqwest::Error → hyper_util legacy Error(Connect)
///                → io::Error(Other, io::Error(InvalidData, rustls::Error))
/// ```
///
/// `std::io::Error::source()` returns its *payload's* source, not the payload,
/// so the `rustls::Error` is reachable only through `io::Error::get_ref()` —
/// twice. ⛔ A `source()`-only walk classifies **every** certificate failure as
/// an ordinary unreachable host, which is the exact false-green this function
/// exists to prevent.
///
/// ⚠ A refused TCP connect is also `is_connect() == true` and carries **no**
/// `rustls::Error`; that is how the two are told apart.
pub fn find_rustls_error<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a rustls::Error> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(found) = error.downcast_ref::<rustls::Error>() {
            return Some(found);
        }
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            if let Some(found) = unwrap_io_payloads(io) {
                return Some(found);
            }
        }
        current = error.source();
    }
    None
}

/// Follow `io::Error::get_ref()` as far as it goes, looking for a `rustls::Error`.
fn unwrap_io_payloads(io: &std::io::Error) -> Option<&rustls::Error> {
    let mut payload = io.get_ref();
    while let Some(inner) = payload {
        if let Some(found) = inner.downcast_ref::<rustls::Error>() {
            return Some(found);
        }
        payload = inner.downcast_ref::<std::io::Error>()?.get_ref();
    }
    None
}

/// Map a `rustls::Error` to its anchor sub-cause, or `None` when it is not an
/// anchor failure at all (`A31`).
///
/// ⛔ Never compares against a specific `CertificateError` with `==`: `Expired`
/// and `ExpiredContext` are **unequal** under that enum's hand-written
/// `PartialEq`, so an equality check silently misses every real expiry (rustls
/// emits the `Context` forms).
///
/// ⛔ Never matches `"CaUsedAsEndEntity"` in a `Debug`/`Display` string: rustls
/// wraps webpki's error as `CertificateError::Other(OtherError(Arc<dyn Error>))`,
/// and the typed downcast is the only honest read of it.
///
/// ⚠ Version-split ratchet: if `rustls` ever resolves a `webpki` our declaration
/// does not unify with, this downcast returns `None` and form 8 silently degrades
/// to form 5. `conformance_a2a_architecture` pins the one-copy declaration and
/// `AC3(h)` asserts the **variant**, not the string.
pub fn anchor_failure_of(error: &rustls::Error) -> Option<AnchorFailure> {
    use rustls::CertificateError;

    let rustls::Error::InvalidCertificate(certificate) = error else {
        return None;
    };
    Some(match certificate {
        CertificateError::Expired
        | CertificateError::ExpiredContext { .. }
        | CertificateError::NotValidYet
        | CertificateError::NotValidYetContext { .. } => AnchorFailure::OutsideValidity,
        CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
            AnchorFailure::WrongName
        }
        CertificateError::Other(other)
            if other.0.as_ref().downcast_ref::<webpki::Error>()
                == Some(&webpki::Error::CaUsedAsEndEntity) =>
        {
            AnchorFailure::CaAsServerCert
        }
        _ => AnchorFailure::Mismatch,
    })
}

/// The fixed discriminant label a client may log for an anchor failure.
///
/// ⛔ Never the rustls error's `Debug`/`Display`: `NotValidForNameContext`
/// carries the names the **peer** presented (`AD-1824`).
pub fn anchor_failure_label(failure: AnchorFailure) -> &'static str {
    match failure {
        AnchorFailure::Mismatch => "Mismatch",
        AnchorFailure::OutsideValidity => "OutsideValidity",
        AnchorFailure::WrongName => "WrongName",
        AnchorFailure::CaAsServerCert => "CaUsedAsEndEntity",
    }
}
