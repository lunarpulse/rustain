use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const SHA2_256_MULTIHASH_CODE: u8 = 0x12;
const SHA2_256_DIGEST_LEN: u8 = 0x20;
const ED25519_PUBLIC_KEY_LEN: usize = 32;

/// RAP peer identifier: a minimal sha2-256 multihash of the Ed25519 public key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct PeerId(String);

impl PeerId {
    pub fn from_public_key(public_key: &[u8]) -> Result<Self, PeerIdentityError> {
        if public_key.len() != ED25519_PUBLIC_KEY_LEN {
            return Err(PeerIdentityError::InvalidPublicKeyLength(public_key.len()));
        }
        let digest = Sha256::digest(public_key);
        let mut bytes = Vec::with_capacity(2 + digest.len());
        bytes.push(SHA2_256_MULTIHASH_CODE);
        bytes.push(SHA2_256_DIGEST_LEN);
        bytes.extend_from_slice(&digest);
        Ok(Self(hex::encode(bytes)))
    }

    pub fn parse(s: impl Into<String>) -> Result<Self, PeerIdentityError> {
        let s = s.into();
        let bytes = hex::decode(&s).map_err(|_| PeerIdentityError::InvalidPeerIdEncoding)?;
        validate_multihash_bytes(&bytes)?;
        // Canonical form is lowercase hex (as produced by `from_public_key`).
        // Reject any noncanonical encoding (uppercase / mixed case) so that two
        // byte-equal peer ids can never collide via case variation and the
        // serialized string is always the single canonical representation.
        if s != hex::encode(&bytes) {
            return Err(PeerIdentityError::NoncanonicalPeerIdEncoding);
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_multihash_bytes(&self) -> Result<Vec<u8>, PeerIdentityError> {
        let bytes = hex::decode(&self.0).map_err(|_| PeerIdentityError::InvalidPeerIdEncoding)?;
        validate_multihash_bytes(&bytes)?;
        Ok(bytes)
    }

    pub fn matches_public_key(&self, public_key: &[u8]) -> bool {
        Self::from_public_key(public_key).is_ok_and(|want| want == *self)
    }

    /// Derive a `PeerId` from a JWK `x` value — a base64url (unpadded) Ed25519
    /// public key, the form an operator pins in `.rustain/a2a.json`.
    ///
    /// Lives here rather than beside `A2aPeerSpec` because "how a public-key
    /// encoding becomes a `PeerId`" is knowledge about `PeerId`, next to the
    /// sha256-multihash-and-hex it already owns. Keeping the codec here also keeps
    /// `a2a_peer_spec.rs` free of any wire encoding, which
    /// `a2a_domain_model_remains_transport_and_wire_free` pins.
    ///
    /// Pure crypto/encoding in `domain/` is deliberate and sanctioned — see
    /// `a2a_wire_types_absent_from_entire_src_domain`.
    pub fn from_jwk_ed25519_x(x: &str) -> Result<Self, PeerIdentityError> {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(x.as_bytes())
            .map_err(|_| PeerIdentityError::InvalidPublicKeyEncoding)?;
        Self::from_public_key(&bytes)
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PeerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        PeerId::parse(s).map_err(serde::de::Error::custom)
    }
}

/// Ed25519 signature bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ed25519Sig(pub Vec<u8>);

impl Ed25519Sig {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Public peer identity carried on signed envelopes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerIdentity {
    pub peer_id: PeerId,
    pub public_key: Vec<u8>,
}

impl PeerIdentity {
    pub fn from_public_key(public_key: Vec<u8>) -> Result<Self, PeerIdentityError> {
        let peer_id = PeerId::from_public_key(&public_key)?;
        Ok(Self {
            peer_id,
            public_key,
        })
    }

    pub fn verify_binding(&self) -> Result<(), PeerIdentityError> {
        if self.peer_id.matches_public_key(&self.public_key) {
            Ok(())
        } else {
            Err(PeerIdentityError::PeerIdPublicKeyMismatch)
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PeerIdentityError {
    #[error("ed25519 public key must be 32 bytes, got {0}")]
    InvalidPublicKeyLength(usize),
    #[error("ed25519 public key is not valid unpadded base64url")]
    InvalidPublicKeyEncoding,
    #[error("peer id is not lowercase hex multihash bytes")]
    InvalidPeerIdEncoding,
    #[error("peer id hex is not canonical lowercase (uppercase or mixed case rejected)")]
    NoncanonicalPeerIdEncoding,
    #[error("peer id multihash must be sha2-256 code 0x12 and 32-byte digest")]
    InvalidPeerIdMultihash,
    #[error("peer id does not match ed25519 public key")]
    PeerIdPublicKeyMismatch,
}

fn validate_multihash_bytes(bytes: &[u8]) -> Result<(), PeerIdentityError> {
    if bytes.len() != 2 + SHA2_256_DIGEST_LEN as usize
        || bytes[0] != SHA2_256_MULTIHASH_CODE
        || bytes[1] != SHA2_256_DIGEST_LEN
    {
        return Err(PeerIdentityError::InvalidPeerIdMultihash);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator-facing pin form: unpadded base64url of a 32-byte Ed25519 key.
    /// Must agree with `from_public_key` on the same bytes, or every pinned peer's
    /// identity would shift (Story 18.3b, ADR-18-3b-01 D1).
    #[test]
    fn from_jwk_ed25519_x_agrees_with_from_public_key() {
        use base64::Engine as _;
        for seed in [0u8, 1, 7, 42, 200, 255] {
            let bytes = [seed; 32];
            let x = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
            assert_eq!(
                PeerId::from_jwk_ed25519_x(&x),
                PeerId::from_public_key(&bytes),
                "pin decode diverged for seed {seed}"
            );
        }
    }

    #[test]
    fn from_jwk_ed25519_x_rejects_non_base64url() {
        assert_eq!(
            PeerId::from_jwk_ed25519_x("not+valid/base64url=="),
            Err(PeerIdentityError::InvalidPublicKeyEncoding)
        );
    }

    /// A well-formed encoding of the wrong LENGTH must fail on length, not be
    /// silently accepted — the caller treats any error as "no usable pin".
    #[test]
    fn from_jwk_ed25519_x_rejects_a_short_key() {
        use base64::Engine as _;
        let short = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1u8; 16]);
        assert_eq!(
            PeerId::from_jwk_ed25519_x(&short),
            Err(PeerIdentityError::InvalidPublicKeyLength(16))
        );
    }

    #[test]
    fn peer_id_round_trips_minimal_sha256_multihash() {
        let public_key = [7u8; 32];
        let peer_id = PeerId::from_public_key(&public_key).expect("peer id");
        let bytes = peer_id.as_multihash_bytes().expect("multihash bytes");
        assert_eq!(bytes[0], SHA2_256_MULTIHASH_CODE);
        assert_eq!(bytes[1], SHA2_256_DIGEST_LEN);
        assert_eq!(PeerId::parse(peer_id.as_str()).unwrap(), peer_id);
        assert!(peer_id.matches_public_key(&public_key));
    }

    #[test]
    fn peer_identity_rejects_pubkey_mismatch() {
        let peer_id = PeerId::from_public_key(&[1u8; 32]).unwrap();
        let identity = PeerIdentity {
            peer_id,
            public_key: vec![2u8; 32],
        };
        assert_eq!(
            identity.verify_binding().unwrap_err(),
            PeerIdentityError::PeerIdPublicKeyMismatch
        );
    }

    #[test]
    fn parse_rejects_noncanonical_uppercase_hex() {
        let public_key = [7u8; 32];
        let canonical = PeerId::from_public_key(&public_key).unwrap();
        // Uppercase decodes to the same bytes but is not the canonical form.
        let upper = canonical.as_str().to_uppercase();
        assert_eq!(
            PeerId::parse(&upper).unwrap_err(),
            PeerIdentityError::NoncanonicalPeerIdEncoding
        );
        // The canonical lowercase form is accepted.
        assert_eq!(PeerId::parse(canonical.as_str()).unwrap(), canonical);
    }

    #[test]
    fn serde_rejects_noncanonical_and_malformed_peer_id() {
        let public_key = [7u8; 32];
        let canonical = PeerId::from_public_key(&public_key).unwrap();
        // Canonical round-trips.
        let json = serde_json::to_string(&canonical).unwrap();
        let back: PeerId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, canonical);

        // Uppercase (same bytes) is rejected at deserialization.
        let upper_json = serde_json::to_string(&canonical.as_str().to_uppercase()).unwrap();
        assert!(serde_json::from_str::<PeerId>(&upper_json).is_err());

        // Malformed (wrong length / bad multihash) rejected.
        assert!(serde_json::from_str::<PeerId>("\"deadbeef\"").is_err());
    }
}
