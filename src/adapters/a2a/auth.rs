//! Non-loopback defence for the A2A front door: bind safety, credential
//! handling, and the per-request authentication gate.
//!
//! Story 18.1b, AC3b. Non-loopback serving is an **atomic unit** — TLS *and*
//! API-key authentication *and* a signed identity, or no socket at all. A
//! plaintext or unauthenticated bind off loopback is refused, not warned about.
//!
//! Two decision cores live here, and they are not interchangeable:
//!
//! * [`evaluate_bind_safety`] decides over the address **string** the operator
//!   typed. It runs before resolution and before any socket exists.
//! * The `anyhow::ensure!` in [`super::server::serve`] re-checks the address the
//!   **kernel** actually gave us. It is the last line of defence against a
//!   caller that hands `serve` a listener it bound itself, and Story 18.1b
//!   *conditions* it on the same TLS+auth evidence rather than deleting it
//!   (R3). Extending only this file would ship a server that passes bind safety
//!   and then refuses to serve.

use std::sync::Arc;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::domain::models::SecretString;

/// The HTTP header the API-key scheme is carried in. Named once, here, because
/// three things must agree on it: the middleware that reads it, the AgentCard
/// `securitySchemes` entry that advertises it, and the operator documentation.
pub const API_KEY_HEADER: &str = "x-api-key";

/// The name the API-key scheme is published under in the AgentCard.
pub const API_KEY_SCHEME_NAME: &str = "apiKey";

/// How this server authenticates inbound requests.
///
/// First cut: API key. mTLS (`DF-18-1-MTLS`) and OAuth2 (`DF-18-1-OAUTH2`)
/// become additional variants **and** additional entries in the card's
/// `securitySchemes` map — a map that now exists, so neither is a card-shape
/// migration.
#[non_exhaustive]
#[derive(Clone)]
pub enum A2aServerAuth {
    /// Shared secrets presented in [`API_KEY_HEADER`].
    ApiKey {
        /// One [`SecretString`] per configured key. The configuration holds
        /// only environment-variable names, and values are never exposed
        /// outside [`A2aServerAuth::verify`].
        keys: Vec<SecretString>,
    },
}

impl std::fmt::Debug for A2aServerAuth {
    /// Hand-written rather than derived. `SecretString`'s own `Debug` already
    /// redacts, but deriving here would put the *variant shape* one field
    /// rename away from leaking, and this type is the thing a tracing span or a
    /// config dump is most likely to print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey { .. } => f
                .debug_struct("A2aServerAuth::ApiKey")
                .field("keys", &"<redacted>")
                .finish(),
        }
    }
}

/// Result of checking one request's credential.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    /// A valid credential was presented.
    Authenticated,
    /// No credential this server recognizes was presented. A first-cut
    /// `Authorization: Bearer …` lands here: OAuth2 is not a scheme we accept
    /// yet, so a bearer token is *absence of a credential*, not a bad one.
    NoCredential,
    /// A credential of the right shape was presented and did not match.
    Rejected,
}

impl A2aServerAuth {
    /// Check a request's credentials.
    ///
    /// `api_key` is the raw [`API_KEY_HEADER`] value; `has_bearer` records that
    /// the request carried `Authorization: Bearer …` so the log line can say
    /// *why* an otherwise well-intentioned OAuth2 client was turned away.
    #[must_use]
    pub fn verify(&self, api_key: Option<&str>, has_bearer: bool) -> AuthOutcome {
        match self {
            Self::ApiKey { keys } => {
                let Some(presented) = api_key.filter(|value| !value.is_empty()) else {
                    if has_bearer {
                        tracing::debug!(
                            "A2A request presented an Authorization: Bearer credential; OAuth2 \
                             is not an accepted scheme (DF-18-1-OAUTH2) — treated as no credential"
                        );
                    }
                    return AuthOutcome::NoCredential;
                };
                if !self.is_configured() {
                    return AuthOutcome::NoCredential;
                }
                if constant_time_secret_eq(keys, presented) {
                    AuthOutcome::Authenticated
                } else {
                    // Names the FACT, never the presented value: a rejected-auth
                    // log line is exactly where a mistyped-but-nearly-correct key
                    // would otherwise be archived in plaintext.
                    tracing::warn!("A2A request rejected: API key did not match");
                    AuthOutcome::Rejected
                }
            }
        }
    }

    /// True only when at least one credential is configured.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        match self {
            Self::ApiKey { keys } => !keys.is_empty(),
        }
    }

    /// The AgentCard `securitySchemes` entry this server actually enforces.
    ///
    /// The card and the middleware read this **same** function, so a card that
    /// advertises a scheme the server does not enforce (or vice versa) is not
    /// expressible (AC7b/R9).
    #[must_use]
    pub fn declared_scheme(&self) -> (&'static str, serde_json::Value) {
        match self {
            Self::ApiKey { .. } => (
                API_KEY_SCHEME_NAME,
                serde_json::json!({
                    "type": "apiKey",
                    "in": "header",
                    "name": API_KEY_HEADER,
                    "description": "Shared API key issued by this instance's operator.",
                }),
            ),
        }
    }
}

/// Constant-time comparison of a presented credential against the configured
/// secret.
///
/// Both sides are hashed to a fixed 32 bytes first, so the comparison is over a
/// constant span and leaks neither the secret's length nor a matching prefix. A
/// naive `==` over a shared secret reachable from a network socket is a timing
/// oracle: `memcmp` returns at the first differing byte, and a patient caller
/// recovers the key one byte at a time.
fn constant_time_secret_eq(secrets: &[SecretString], presented: &str) -> bool {
    let actual = Sha256::digest(presented.as_bytes());
    let mut matched = 0_u8;
    for secret in secrets {
        matched |= Sha256::digest(secret.expose_secret().as_bytes())
            .ct_eq(&actual)
            .unwrap_u8();
    }
    matched == 1
}

/// TLS material for a non-loopback listener. Opaque on purpose: the rest of the
/// server only ever asks *whether* it exists.
#[derive(Clone)]
pub struct A2aTlsMaterial {
    pub(crate) config: Arc<rustls::ServerConfig>,
}

impl std::fmt::Debug for A2aTlsMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("A2aTlsMaterial(<rustls::ServerConfig>)")
    }
}

/// Everything the non-loopback gate needs, in one value.
///
/// Defaults to "nothing configured", which is precisely the loopback-only
/// posture Story 18.1a shipped.
#[derive(Clone, Debug, Default)]
pub struct A2aServerSecurity {
    pub tls: Option<A2aTlsMaterial>,
    pub auth: Option<A2aServerAuth>,
}

impl A2aServerSecurity {
    /// True only when the full non-loopback unit is present.
    #[must_use]
    pub fn is_network_ready(&self) -> bool {
        self.tls.is_some() && self.auth.as_ref().is_some_and(A2aServerAuth::is_configured)
    }
}

/// The three pieces of evidence a non-loopback bind requires, as plain bools so
/// the decision core stays pure and exhaustively testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BindEvidence {
    pub tls: bool,
    pub api_key_auth: bool,
    pub signed_identity: bool,
}

impl BindEvidence {
    /// Evidence derived from a configured [`A2aServerSecurity`] plus whether a
    /// signed local identity loaded.
    #[must_use]
    pub fn from_security(security: &A2aServerSecurity, signed_identity: bool) -> Self {
        Self {
            tls: security.tls.is_some(),
            api_key_auth: security
                .auth
                .as_ref()
                .is_some_and(A2aServerAuth::is_configured),
            signed_identity,
        }
    }

    fn missing(self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.tls {
            missing.push("TLS (`server.tls.cert` + `server.tls.key`)");
        }
        if !self.api_key_auth {
            missing.push("API-key authentication (`server.api_key_env`)");
        }
        if !self.signed_identity {
            missing.push("a signed local identity");
        }
        missing
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindDecision {
    Bind,
    RefuseWithReason(String),
}

/// Effect-free bind decision over the address **string**.
///
/// DNS is deliberately not consulted: the loopback set and the exact
/// `localhost` hostname are accepted literally, and everything else is
/// non-loopback and therefore subject to the atomic TLS+auth+identity unit.
/// Resolution happens in the shell, which then binds a concrete address it has
/// checked — see [`super::server::run`].
#[must_use]
pub fn evaluate_bind_safety(addr: &str, evidence: BindEvidence) -> BindDecision {
    let parsed = match url::Url::parse(&format!("http://{addr}")) {
        Ok(parsed)
            if parsed.port().is_some()
                && parsed.path() == "/"
                && parsed.query().is_none()
                && parsed.fragment().is_none()
                && parsed.username().is_empty()
                && parsed.password().is_none() =>
        {
            parsed
        }
        _ => {
            return BindDecision::RefuseWithReason(
                "A2A bind must be a host and an explicit port".to_owned(),
            );
        }
    };
    let is_loopback = match parsed.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if is_loopback {
        // Loopback stays plaintext and unauthenticated (Story 18.1a): an
        // attacker who can reach it already runs code on this machine.
        return BindDecision::Bind;
    }
    let missing = evidence.missing();
    if missing.is_empty() {
        BindDecision::Bind
    } else {
        BindDecision::RefuseWithReason(format!(
            "refusing to serve A2A on non-loopback address {addr}: non-loopback serving requires \
             TLS, API-key authentication and a signed identity together; missing {}",
            missing.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: BindEvidence = BindEvidence {
        tls: true,
        api_key_auth: true,
        signed_identity: true,
    };

    #[test]
    fn loopback_binds_without_any_evidence() {
        for addr in ["127.0.0.1:8080", "[::1]:8080", "localhost:8080"] {
            assert_eq!(
                evaluate_bind_safety(addr, BindEvidence::default()),
                BindDecision::Bind,
                "{addr}"
            );
        }
    }

    #[test]
    fn non_loopback_binds_only_with_the_whole_unit() {
        assert_eq!(
            evaluate_bind_safety("0.0.0.0:8443", FULL),
            BindDecision::Bind
        );
        // Every proper subset of the evidence must refuse — the unit is atomic,
        // so "TLS but no auth" is not a partial win, it is a refusal.
        for drop in 0..3 {
            let mut evidence = FULL;
            match drop {
                0 => evidence.tls = false,
                1 => evidence.api_key_auth = false,
                _ => evidence.signed_identity = false,
            }
            assert!(
                matches!(
                    evaluate_bind_safety("0.0.0.0:8443", evidence),
                    BindDecision::RefuseWithReason(_)
                ),
                "evidence {evidence:?} must not bind"
            );
        }
    }

    #[test]
    fn a_refusal_names_every_missing_component() {
        let BindDecision::RefuseWithReason(reason) =
            evaluate_bind_safety("203.0.113.7:8443", BindEvidence::default())
        else {
            panic!("must refuse");
        };
        assert!(reason.contains("TLS"), "{reason}");
        assert!(reason.contains("API-key"), "{reason}");
        assert!(reason.contains("signed"), "{reason}");
    }

    #[test]
    fn malformed_authorities_are_refused_regardless_of_evidence() {
        for addr in ["127.0.0.1", "user:pw@127.0.0.1:1/x", "127.0.0.1:1?q=1", ""] {
            assert!(
                matches!(
                    evaluate_bind_safety(addr, FULL),
                    BindDecision::RefuseWithReason(_)
                ),
                "{addr:?}"
            );
        }
    }

    fn api_key_auth(key: &str) -> A2aServerAuth {
        A2aServerAuth::ApiKey {
            keys: vec![SecretString::from(key)],
        }
    }

    fn api_keys_auth(keys: &[&str]) -> A2aServerAuth {
        A2aServerAuth::ApiKey {
            keys: keys.iter().map(|key| SecretString::from(*key)).collect(),
        }
    }

    #[test]
    fn configured_keys_authenticate_as_one_accepted_set() {
        let auth = api_key_auth("s3cret-key");
        assert_eq!(
            auth.verify(Some("s3cret-key"), false),
            AuthOutcome::Authenticated
        );
        assert_eq!(auth.verify(Some("s3cret-ke"), false), AuthOutcome::Rejected);
        assert_eq!(
            auth.verify(Some("s3cret-keyy"), false),
            AuthOutcome::Rejected
        );
        assert_eq!(auth.verify(Some(""), false), AuthOutcome::NoCredential);
        assert_eq!(auth.verify(None, false), AuthOutcome::NoCredential);

        let multi_key = api_keys_auth(&["first-key", "second-key"]);
        assert_eq!(
            multi_key.verify(Some("first-key"), false),
            AuthOutcome::Authenticated
        );
        assert_eq!(
            multi_key.verify(Some("second-key"), false),
            AuthOutcome::Authenticated
        );
        assert_eq!(
            multi_key.verify(Some("third-key"), false),
            AuthOutcome::Rejected
        );
        assert!(
            !api_keys_auth(&[]).is_configured(),
            "an empty key vector must not make a network listener ready"
        );
    }

    #[test]
    fn a_bearer_token_is_no_credential_not_a_wrong_one() {
        // OAuth2 is deferred (DF-18-1-OAUTH2). The distinction matters: a
        // `Rejected` outcome tells a client its key is wrong, which would be a
        // lie for a client that never sent a key.
        let auth = api_key_auth("s3cret-key");
        assert_eq!(auth.verify(None, true), AuthOutcome::NoCredential);
    }

    #[test]
    fn debug_rendering_never_carries_key_material() {
        // R6: the auth config is the value most likely to reach a tracing span
        // or a `{:?}` config dump.
        let auth = api_key_auth("s3cret-key");
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("s3cret-key"), "{rendered}");
        let security = A2aServerSecurity {
            tls: None,
            auth: Some(auth),
        };
        let rendered = format!("{security:?}");
        assert!(!rendered.contains("s3cret-key"), "{rendered}");
    }

    #[test]
    fn comparison_is_constant_time_by_construction() {
        // R6 asserts the PRIMITIVE'S CONTRACT, never a wall-clock measurement:
        // a timing assertion in CI is a coin flip, and passing one is not
        // evidence the comparison is safe.
        //
        // The contract has two halves, both checked here:
        //  1. both operands are hashed to a fixed 32 bytes, so the compared span
        //     does not depend on the secret; and
        //  2. the compare is `subtle::ConstantTimeEq`, whose documented contract
        //     is a branch-free, non-short-circuiting fold.
        let secret = SecretString::from("s3cret-key");
        assert_eq!(Sha256::digest("s3cret-key".as_bytes()).len(), 32);
        assert_eq!(Sha256::digest("".as_bytes()).len(), 32);
        assert!(constant_time_secret_eq(
            std::slice::from_ref(&secret),
            "s3cret-key"
        ));
        // A shared prefix must not be distinguishable from a total mismatch at
        // the API level, which is what the fixed-width digest guarantees.
        assert!(!constant_time_secret_eq(
            std::slice::from_ref(&secret),
            "s3cret-ke"
        ));
        assert!(!constant_time_secret_eq(
            std::slice::from_ref(&secret),
            "zzzzzzzzzz"
        ));
        let equal: bool = Sha256::digest(b"a").ct_eq(&Sha256::digest(b"a")).into();
        assert!(equal);
    }

    #[test]
    fn the_card_declares_the_scheme_the_middleware_reads() {
        let (name, scheme) = api_key_auth("k").declared_scheme();
        assert_eq!(name, API_KEY_SCHEME_NAME);
        assert_eq!(scheme["type"], "apiKey");
        assert_eq!(scheme["in"], "header");
        assert_eq!(scheme["name"], API_KEY_HEADER);
    }
}
