use serde::{Deserialize, Serialize};

use crate::domain::models::redacted_url::RedactedUrl;

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
}

impl A2aPeerSpecError {
    pub fn algorithm(&self) -> Option<&str> {
        match self {
            Self::UnsupportedPinnedAlgorithm { algorithm } => Some(algorithm),
            _ => None,
        }
    }
}
