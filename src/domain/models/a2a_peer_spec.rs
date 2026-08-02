use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::models::redacted_url::RedactedUrl;
use crate::domain::models::{PeerId, PeerIdentityError};

/// Trust assigned by operator configuration, never by an AgentCard payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    Verified,
    Unverified,
}

/// Signature algorithm accepted for a pinned AgentCard key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PinnedKeyAlgorithm {
    #[serde(rename = "EdDSA")]
    EdDsa,
}

/// Operator-pinned public key in JWK `x` form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedKey {
    pub alg: PinnedKeyAlgorithm,
    pub x: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kid: Option<String>,
}

impl PinnedKey {
    pub fn new(alg: PinnedKeyAlgorithm, x: String, kid: Option<String>) -> Self {
        Self { alg, x, kid }
    }

    pub fn parse(
        algorithm: &str,
        x: String,
        kid: Option<String>,
    ) -> Result<Self, A2aPeerSpecError> {
        let alg = match algorithm {
            "EdDSA" => PinnedKeyAlgorithm::EdDsa,
            other => {
                return Err(A2aPeerSpecError::UnsupportedPinnedAlgorithm {
                    algorithm: other.to_owned(),
                });
            }
        };
        if x.trim().is_empty() {
            return Err(A2aPeerSpecError::EmptyPinnedKey);
        }
        PeerId::from_jwk_ed25519_x(&x)
            .map_err(|source| A2aPeerSpecError::InvalidPinnedKey { source })?;
        Ok(Self::new(alg, x, kid))
    }
}

/// Provenance of an A2A peer specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum A2aPeerSource {
    /// Came from workspace `.rustain/a2a.json`.
    Workspace,
    /// Came from the active profile's `[tools.config.a2a.<peer>]` table.
    Profile { profile_name: String },
}

/// An allowlisted A2A peer. The optional pin is the sole trust-tier source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aPeerSpec {
    pub id: String,
    pub url: RedactedUrl,
    pub pinned_key: Option<PinnedKey>,
    pub source: A2aPeerSource,
}

impl A2aPeerSpec {
    pub fn trust_tier(&self) -> TrustTier {
        if self.pinned_key.is_some() {
            TrustTier::Verified
        } else {
            TrustTier::Unverified
        }
    }

    pub fn validate_id(&self) -> Result<(), A2aPeerSpecError> {
        if self.id.trim().is_empty() {
            return Err(A2aPeerSpecError::EmptyId);
        }
        if self.id.contains("__") {
            return Err(A2aPeerSpecError::ReservedId {
                id: self.id.clone(),
            });
        }
        Ok(())
    }

    /// The cryptographic identity implied by the operator's pin, if any.
    ///
    /// Config parsing validates every authored pin. `None` therefore means
    /// unpinned in production; programmatically constructed invalid specs remain
    /// distinguishable by checking `pinned_key.is_some()`.
    ///
    /// This is the sole identity derivation in the tree. It lives in the domain
    /// — rather than in `adapters::a2a`, where it began as the private
    /// `driver.rs::peer_id_for` — because Story 18.3b's policy resolution must
    /// not require the `a2a` feature, for the same reason `adapters/a2a/config.rs`
    /// documents for peer parsing: a misconfigured build should fail loudly at
    /// startup rather than silently ignore the operator's intent.
    ///
    /// Note it is deliberately **not** derived from `AgentId`. A peer node id is
    /// `a2a/p-<b64(alias)>/…`, `a2a-in/p-<b64(submitter)>/…`, or
    /// `<peer_id_hex>/agent` depending on the route, so its first segment is a
    /// namespace tag on two of the three production paths.
    pub fn pinned_identity(&self) -> Option<PeerId> {
        PeerId::from_jwk_ed25519_x(&self.pinned_key.as_ref()?.x).ok()
    }

    /// A pseudonym derived from the alias, for a peer with no usable pin.
    ///
    /// **Not rename-stable** — which is the reason it is named separately rather
    /// than reached through an `unwrap_or_else`. Renaming the alias yields a
    /// different pseudonym, and a per-sender policy override silently riding on
    /// that is an authorization change disguised as a config edit.
    pub fn alias_pseudonym(&self) -> PeerId {
        alias_pseudonym(&self.id)
    }

    /// The identity a peer resolves to today: the pin when there is one, else the
    /// alias pseudonym.
    ///
    /// Callers that must distinguish the two cases (policy resolution, the NFR66
    /// explainer) use [`Self::pinned_identity`] directly.
    pub fn resolved_identity(&self) -> PeerId {
        self.pinned_identity()
            .unwrap_or_else(|| self.alias_pseudonym())
    }
}

/// `sha256(alias)` as a `PeerId`, for an alias with no pin behind it.
pub fn alias_pseudonym(alias: &str) -> PeerId {
    PeerId::from_public_key(&Sha256::digest(alias.as_bytes()))
        .expect("sha256 digest is exactly 32 bytes")
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum A2aPeerSpecError {
    #[error("A2A peer id must not be empty or whitespace")]
    EmptyId,
    #[error(
        "A2A peer id {id:?} must not contain double-underscore (__); it conflicts with the a2a::<peer>::<skill> naming convention"
    )]
    ReservedId { id: String },
    #[error(
        "unsupported pinned-key algorithm {algorithm:?}; remove the pin to accept this peer unverified, or wait for DF-17-4a-2"
    )]
    UnsupportedPinnedAlgorithm { algorithm: String },
    #[error("pinned Ed25519 JWK x value must not be empty")]
    EmptyPinnedKey,
    #[error("pinned Ed25519 JWK x value is invalid: {source}")]
    InvalidPinnedKey {
        #[source]
        source: PeerIdentityError,
    },
}

impl A2aPeerSpecError {
    pub fn algorithm(&self) -> Option<&str> {
        match self {
            Self::UnsupportedPinnedAlgorithm { algorithm } => Some(algorithm),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::PinnedKeyAlgorithm;

    // Pins as an operator writes them: unpadded base64url of a 32-byte Ed25519 key.
    //
    // Held as literals rather than encoded in-test on purpose — the
    // `a2a_domain_model_remains_transport_and_wire_free` guard
    // (`domain/ports/capability_provider.rs:80`) pins that THIS FILE names no wire
    // encoding, and a test-only import of the base64 crate would trip it just as a production
    // one would. Decoding lives with `PeerId::from_jwk_ed25519_x`, where the sibling
    // guard explicitly sanctions it.
    const PIN_7: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc";
    const PIN_9: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk";
    const PIN_1: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE";

    fn spec(id: &str, pin: Option<&str>) -> A2aPeerSpec {
        A2aPeerSpec {
            id: id.to_owned(),
            url: RedactedUrl::new("https://peer.example/a2a".to_owned()),
            pinned_key: pin.map(|x| PinnedKey::new(PinnedKeyAlgorithm::EdDsa, x.to_owned(), None)),
            source: A2aPeerSource::Workspace,
        }
    }

    #[test]
    fn pinned_identity_is_the_key_derived_peer_id() {
        assert_eq!(
            spec("marcus-arch", Some(PIN_7)).pinned_identity(),
            Some(PeerId::from_public_key(&[7u8; 32]).unwrap()),
            "the pin literal must decode to the same 32-byte key it encodes"
        );
    }

    /// Story 18.3b AC3 — the identity must not depend on the alias, which is what
    /// lets a per-sender override survive a rename in `.rustain/a2a.json`.
    #[test]
    fn pinned_identity_survives_an_alias_rename() {
        let before = spec("marcus-arch", Some(PIN_9));
        let after = spec("marcus", Some(PIN_9));
        assert_eq!(before.pinned_identity(), after.pinned_identity());
        assert_ne!(
            before.alias_pseudonym(),
            after.alias_pseudonym(),
            "the alias pseudonym must NOT be rename-stable — that is why it is named separately"
        );
    }

    #[test]
    fn unpinned_peer_has_no_pinned_identity() {
        let unpinned = spec("drive-by", None);
        assert_eq!(unpinned.pinned_identity(), None);
        assert_eq!(unpinned.trust_tier(), TrustTier::Unverified);
        assert_eq!(unpinned.resolved_identity(), unpinned.alias_pseudonym());
    }

    /// A pin that does not decode must stay `None` rather than fall through to the
    /// alias pseudonym — the silent-fallback defect the domain move removes.
    #[test]
    fn malformed_pin_yields_no_identity_rather_than_a_pseudonym() {
        let mut malformed = spec("typo", Some(PIN_1));
        malformed.pinned_key.as_mut().unwrap().x = "tooshort".to_owned();
        assert_eq!(malformed.pinned_identity(), None);
        assert_eq!(
            malformed.trust_tier(),
            TrustTier::Verified,
            "a present-but-malformed pin still reads as Verified, which is exactly \
             why pinned_identity must report the failure instead of substituting"
        );
    }
    #[test]
    fn malformed_pin_is_rejected_at_the_configuration_boundary() {
        let error = PinnedKey::parse("EdDSA", "tooshort".to_owned(), None)
            .expect_err("invalid Ed25519 key material must not become a peer spec");
        assert!(matches!(error, A2aPeerSpecError::InvalidPinnedKey { .. }));
    }

    /// The promoted derivation must agree byte-for-byte with the deleted
    /// `a2a/driver.rs::peer_id_for`, or every pinned peer's identity would shift.
    #[test]
    fn resolved_identity_matches_the_prior_private_derivation() {
        let pinned = spec("planets", Some(PIN_7));
        assert_eq!(
            pinned.resolved_identity(),
            PeerId::from_public_key(&[7u8; 32]).unwrap()
        );
        let unpinned = spec("planets", None);
        assert_eq!(
            unpinned.resolved_identity(),
            PeerId::from_public_key(&Sha256::digest("planets".as_bytes())).unwrap()
        );
    }
}
