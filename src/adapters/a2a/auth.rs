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

use std::collections::BTreeSet;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq};
use zeroize::Zeroizing;

use super::config::A2aServerConfig;
use super::exec::SubmitterKey;
use crate::domain::models::{A2aPeerSpec, PeerId, SecretString};

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
///
/// Story 19.18d `A16` — a tuple variant over the opaque [`ApiKeySet`]: enum
/// variant fields are always public, so a struct-like variant would let any
/// caller hand-build a binding or an adoption scope. Bound slots come only from
/// [`resolve_server_auth`].
#[non_exhaustive]
#[derive(Clone)]
pub enum A2aServerAuth {
    /// Shared secrets presented in [`API_KEY_HEADER`].
    ApiKey(ApiKeySet),
}

impl std::fmt::Debug for A2aServerAuth {
    /// Hand-written rather than derived: this type is the thing a tracing span
    /// or a config dump is most likely to print, and a derive would put a
    /// digest one field rename away from a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(set) => f
                .debug_struct("A2aServerAuth::ApiKey")
                .field("keys", set)
                .finish(),
        }
    }
}

/// Story 19.18d `A3` — the configured keys, held as SHA-256 digests (⛔ never
/// the value), one slot per configured environment-variable name, each with
/// the peer binding its name carries in `server.peerKeys`, if any.
///
/// Private fields: [`Self::unbound`] is the only public constructor; a
/// binding exists only when [`resolve_server_auth`] validated it against the
/// roster.
#[derive(Clone)]
pub struct ApiKeySet {
    slots: Vec<ConfiguredKey>,
}

impl std::fmt::Debug for ApiKeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeySet")
            .field("slots", &self.slots)
            .finish()
    }
}

#[derive(Clone)]
struct ConfiguredKey {
    digest: Zeroizing<[u8; 32]>,
    binding: Option<KeyBinding>,
}

impl std::fmt::Debug for ConfiguredKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfiguredKey")
            .field("digest", &"<redacted>")
            .field("binding", &self.binding)
            .finish()
    }
}

impl ConfiguredKey {
    fn new(secret: &SecretString, binding: Option<KeyBinding>) -> Self {
        Self {
            digest: digest_of(secret),
            binding,
        }
    }
}

/// Story 19.18d `A7`/`P2` — what a bound slot asserts: the operator says the
/// holder of this key is the pinned roster peer `alias`. Possession of the
/// key, never the person (`B1`). Constructed only by [`resolve_server_auth`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBinding {
    alias: String,
    peer: PeerId,
    adopt_pre_binding: bool,
}

impl KeyBinding {
    /// The roster alias this slot was bound through (display attribution).
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// The pinned identity of that alias: `hex(0x12 0x20 ‖ sha256(pubkey))`.
    #[must_use]
    pub fn peer(&self) -> &PeerId {
        &self.peer
    }

    /// Whether this binding reads the key's pre-binding items and its tasks
    /// recovered at the restart that applied the binding (`B3`; Story 19.18e).
    #[must_use]
    pub fn adopt_pre_binding(&self) -> bool {
        self.adopt_pre_binding
    }
}

impl ApiKeySet {
    /// A set with no peer bindings: every key keeps the legacy pseudonym path.
    #[must_use]
    pub fn unbound(keys: Vec<SecretString>) -> Self {
        Self {
            slots: keys
                .iter()
                .map(|key| ConfiguredKey::new(key, None))
                .collect(),
        }
    }

    /// The pseudonymous submitters whose pre-binding items — and, Story 19.18e,
    /// whose tasks recovered at the restart that applied the binding — every
    /// key bound to `pin` may read (`A11`): the key of each slot bound to `pin`
    /// **with** `adoptPreBinding`, deduplicated by value, in slot order.
    #[must_use]
    pub fn adopted_for(&self, pin: &PeerId) -> Vec<SubmitterKey> {
        let mut seen = BTreeSet::new();
        self.slots
            .iter()
            .filter(|slot| {
                slot.binding
                    .as_ref()
                    .is_some_and(|binding| binding.adopt_pre_binding && &binding.peer == pin)
            })
            .filter(|slot| seen.insert(*slot.digest))
            .map(|slot| SubmitterKey::from_api_key_digest(&slot.digest))
            .collect()
    }

    /// The pseudonymous submitter of every unbound slot, deduplicated by value.
    #[must_use]
    pub fn unbound_submitters(&self) -> Vec<SubmitterKey> {
        let mut seen = BTreeSet::new();
        self.slots
            .iter()
            .filter(|slot| slot.binding.is_none())
            .filter(|slot| seen.insert(*slot.digest))
            .map(|slot| SubmitterKey::from_api_key_digest(&slot.digest))
            .collect()
    }

    /// Every pin at least one slot is bound to.
    #[must_use]
    pub fn bound_pins(&self) -> BTreeSet<PeerId> {
        self.slots
            .iter()
            .filter_map(|slot| slot.binding.as_ref().map(|binding| binding.peer.clone()))
            .collect()
    }

    /// How many distinct key **values** are configured unbound (`A18`).
    #[must_use]
    pub fn distinct_unbound_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.binding.is_none())
            .map(|slot| *slot.digest)
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// Whether any slot carries a peer binding.
    #[must_use]
    pub fn has_bindings(&self) -> bool {
        self.slots.iter().any(|slot| slot.binding.is_some())
    }
}

/// Result of checking one request's credential.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    /// A valid credential was presented; `slot` is the configured key it
    /// matched (Story 19.18d `A2`), resolved with [`A2aServerAuth::binding`].
    Authenticated { slot: u32 },
    /// No credential this server recognizes was presented. A first-cut
    /// `Authorization: Bearer …` lands here: OAuth2 is not a scheme we accept
    /// yet, so a bearer token is *absence of a credential*, not a bad one.
    NoCredential,
    /// A credential of the right shape was presented and did not match.
    Rejected,
}

#[cfg(test)]
thread_local! {
    /// Story 19.18d `K01` — one tick per configured-digest comparison, so the
    /// scan's completeness is a deterministic count (Rule 4).
    static DIGEST_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl A2aServerAuth {
    /// Check a request's credentials.
    ///
    /// `api_key` is the raw [`API_KEY_HEADER`] value; `has_bearer` records that
    /// the request carried `Authorization: Bearer …` so the log line can say
    /// *why* an otherwise well-intentioned OAuth2 client was turned away.
    ///
    /// Story 19.18d `A2`/`P5` — every configured digest is compared with
    /// `subtle::ConstantTimeEq` against `sha256(presented)`, the matching slot
    /// is selected with `conditional_assign`, and the scan never exits early.
    /// That is the claim ceiling: no wall-clock or branch-freedom property is
    /// claimed.
    #[must_use]
    pub fn verify(&self, api_key: Option<&str>, has_bearer: bool) -> AuthOutcome {
        match self {
            Self::ApiKey(set) => {
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
                let actual = Sha256::digest(presented.as_bytes());
                let mut slot = 0_u32;
                let mut found = Choice::from(0);
                for (index, key) in set.slots.iter().enumerate() {
                    #[cfg(test)]
                    DIGEST_COMPARISONS.with(|count| count.set(count.get() + 1));
                    let matched = key.digest.as_slice().ct_eq(actual.as_slice());
                    slot.conditional_assign(&(index as u32), matched);
                    found |= matched;
                }
                if bool::from(found) {
                    AuthOutcome::Authenticated { slot }
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

    /// The binding of the slot [`Self::verify`] selected, or `None` for an
    /// unbound slot.
    #[must_use]
    pub fn binding(&self, slot: u32) -> Option<&KeyBinding> {
        match self {
            Self::ApiKey(set) => set.slots.get(usize::try_from(slot).ok()?)?.binding.as_ref(),
        }
    }

    /// The configured key set.
    #[must_use]
    pub fn keys(&self) -> &ApiKeySet {
        match self {
            Self::ApiKey(set) => set,
        }
    }

    /// True only when at least one credential is configured — bound or not
    /// (Story 19.18d `A15`: a bound-only configuration is network-ready).
    #[must_use]
    pub fn is_configured(&self) -> bool {
        match self {
            Self::ApiKey(set) => !set.slots.is_empty(),
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
            Self::ApiKey(_) => (
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

/// Story 19.18d `A3` — the file's one exposure of a configured key: it is
/// hashed to a fixed 32 bytes at once, so the plaintext never reaches a
/// comparison, a log line or a `Debug`. Every later value comparison (slot
/// selection, `R4`, the distinct-unbound count, the adoption pseudonym) is
/// over digests.
fn digest_of(secret: &SecretString) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(Sha256::digest(secret.expose_secret().as_bytes()).into())
}

/// Story 19.18d `A16` — the one production resolver of the inbound key set:
/// configured names (`apiKeyEnv` ∪ `apiKeys` unbound, `peerKeys` bound) →
/// validated against the roster → values read through `env` → digest slots.
///
/// Refusals run in a fixed order and a config with several faults reports the
/// first (`A10`): `R3` → `R1` → `R2` → `R5` → unset env → `R4`; within one
/// check, env names in sorted order. Messages name env-var **names** and
/// aliases, ⛔ never values.
///
/// `Ok(None)` when no key is configured.
///
/// # Errors
///
/// The first refusal, as an operator-facing message.
pub fn resolve_server_auth(
    config: &A2aServerConfig,
    roster: &[A2aPeerSpec],
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Option<A2aServerAuth>> {
    // The legacy name remains valid and the additional names are a set union:
    // duplicate env-var names do not create duplicate comparisons or principals.
    let mut unbound = BTreeSet::new();
    if let Some(name) = config
        .api_key_env
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        unbound.insert(name.to_owned());
    }
    for name in config.api_keys.as_deref().unwrap_or_default() {
        if !name.is_empty() {
            unbound.insert(name.clone());
        }
    }

    // R3 — a name is either bound or unbound.
    for env in config.peer_keys.keys() {
        if unbound.contains(env) {
            anyhow::bail!(
                "A2A API-key configuration names {env} in both server.peerKeys and \
                 server.apiKeyEnv/apiKeys; a key is either bound to one peer or unbound"
            );
        }
    }
    // R1 — the alias names a roster peer.
    let mut specs = Vec::with_capacity(config.peer_keys.len());
    for (env, binding) in &config.peer_keys {
        let alias = binding.alias();
        let Some(spec) = roster.iter().find(|spec| spec.id == alias) else {
            anyhow::bail!(
                "A2A server.peerKeys binds {env} to {alias}, but no A2A peer named {alias} is \
                 configured"
            );
        };
        specs.push((env, binding, spec));
    }
    // R2 — that peer is pinned (`B2`).
    let mut bound = Vec::with_capacity(specs.len());
    for (env, binding, spec) in specs {
        let alias = binding.alias();
        let Some(peer) = spec.pinned_identity() else {
            anyhow::bail!(
                "A2A server.peerKeys binds {env} to {alias}, but {alias} has no valid pinnedKey; a \
                 key can be bound only to a pinned peer"
            );
        };
        bound.push((
            env.clone(),
            KeyBinding {
                alias: alias.to_owned(),
                peer,
                adopt_pre_binding: binding.adopt_pre_binding(),
            },
        ));
    }
    // R5 — `requirePeerBinding` tolerates no unbound key.
    if config.require_peer_binding
        && let Some(env) = unbound.iter().next()
    {
        anyhow::bail!(
            "A2A server.requirePeerBinding is set, but {env} is not bound to a peer in \
             server.peerKeys"
        );
    }

    let mut names: Vec<(String, Option<KeyBinding>)> = unbound
        .into_iter()
        .map(|name| (name, None))
        .chain(
            bound
                .into_iter()
                .map(|(name, binding)| (name, Some(binding))),
        )
        .collect();
    if names.is_empty() {
        return Ok(None);
    }
    names.sort_by(|left, right| left.0.cmp(&right.0));

    // Unset/empty env — today's message.
    let mut slots = Vec::with_capacity(names.len());
    for (name, binding) in &names {
        let value = env(name).filter(|value| !value.is_empty()).ok_or_else(|| {
            anyhow::anyhow!(
                "A2A API-key configuration names {name}, but that environment variable is unset \
                 or empty"
            )
        })?;
        slots.push(ConfiguredKey::new(
            &SecretString::from(value),
            binding.clone(),
        ));
    }
    // R4 — one key value identifies one binding (`P2`): identical metadata
    // keeps one slot per name; anything else refuses.
    for (left, (env_a, _)) in names.iter().enumerate() {
        for (right, (env_b, _)) in names.iter().enumerate().skip(left + 1) {
            if *slots[left].digest == *slots[right].digest
                && slots[left].binding != slots[right].binding
            {
                anyhow::bail!(
                    "A2A API-key configuration: {env_a} and {env_b} hold the same key but do not \
                     name the same binding; one key value cannot identify two peers or aliases"
                );
            }
        }
    }
    Ok(Some(A2aServerAuth::ApiKey(ApiKeySet { slots })))
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
    /// Story 19.18 `AC12` — thread access for API-key callers, carried with
    /// the credential posture it qualifies. Defaults to off.
    pub thread_reads: super::config::ThreadReads,
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
        A2aServerAuth::ApiKey(ApiKeySet::unbound(vec![SecretString::from(key)]))
    }

    fn api_keys_auth(keys: &[&str]) -> A2aServerAuth {
        A2aServerAuth::ApiKey(ApiKeySet::unbound(
            keys.iter().map(|key| SecretString::from(*key)).collect(),
        ))
    }

    #[test]
    fn configured_keys_authenticate_as_one_accepted_set() {
        let auth = api_key_auth("s3cret-key");
        assert_eq!(
            auth.verify(Some("s3cret-key"), false),
            AuthOutcome::Authenticated { slot: 0 }
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
            AuthOutcome::Authenticated { slot: 0 }
        );
        assert_eq!(
            multi_key.verify(Some("second-key"), false),
            AuthOutcome::Authenticated { slot: 1 }
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

    /// Story 19.18d `K01` (Rule 4 structural ratchet) — `verify` compares
    /// every configured digest, whichever slot matches and when none does, and
    /// returns the matching slot. `M01` (early exit on the first match) and
    /// `M02` (`position` over `==`, no `ct_eq` tick) change the count; `M03`
    /// (select a non-matching slot) changes the slot.
    #[test]
    fn k01_verify_compares_every_digest_and_selects_the_matching_slot() {
        let auth = api_keys_auth(&["slot-zero", "slot-one", "slot-two"]);
        for (presented, expected) in [
            ("slot-zero", AuthOutcome::Authenticated { slot: 0 }),
            ("slot-two", AuthOutcome::Authenticated { slot: 2 }),
            ("slot-unknown", AuthOutcome::Rejected),
        ] {
            DIGEST_COMPARISONS.with(|count| count.set(0));
            assert_eq!(auth.verify(Some(presented), false), expected, "{presented}");
            assert_eq!(
                DIGEST_COMPARISONS.with(std::cell::Cell::get),
                3,
                "every configured digest is compared for {presented}"
            );
        }
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
        // or a `{:?}` config dump. Story 19.18d `AC1(c)`: a bound slot's
        // rendering carries neither the value nor its digest.
        use base64::Engine as _;
        let roster = [crate::domain::models::A2aPeerSpec::new(
            "p1",
            crate::domain::models::RedactedUrl::new("https://p1.example/".to_owned()),
            crate::domain::models::A2aPeerSource::Workspace,
        )
        .with_pinned_key(Some(crate::domain::models::PinnedKey::new(
            crate::domain::models::PinnedKeyAlgorithm::EdDsa,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([9_u8; 32]),
            None,
        )))];
        let config: A2aServerConfig = serde_json::from_value(serde_json::json!({
            "apiKeys": ["UNBOUND_KEY"],
            "peerKeys": { "BOUND_KEY": "p1" },
        }))
        .unwrap();
        let auth = resolve_server_auth(&config, &roster, |name| {
            Some(
                match name {
                    "UNBOUND_KEY" => "s3cret-key",
                    _ => "b0und-key",
                }
                .to_owned(),
            )
        })
        .unwrap()
        .unwrap();
        let rendered = format!("{auth:?}");
        for needle in ["s3cret-key", "b0und-key"] {
            assert!(!rendered.contains(needle), "{rendered}");
            let digest = hex::encode(Sha256::digest(needle.as_bytes()));
            assert!(!rendered.contains(&digest), "{rendered}");
            let bytes = format!("{:?}", Sha256::digest(needle.as_bytes()).as_slice());
            assert!(!rendered.contains(&bytes), "{rendered}");
        }
        assert!(
            rendered.contains("p1"),
            "the binding stays legible: {rendered}"
        );
        let security = A2aServerSecurity {
            tls: None,
            auth: Some(auth),
            ..A2aServerSecurity::default()
        };
        let rendered = format!("{security:?}");
        assert!(!rendered.contains("s3cret-key"), "{rendered}");
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
