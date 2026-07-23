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
