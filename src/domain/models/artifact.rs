//! Durable, content-addressed evidence handles for orchestration rooms.
//!
//! Bodies are deliberately absent from these types. They live behind the
//! `ArtifactStore` port; the journal carries only verified handles.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::domain::models::agent_id::AgentId;
use crate::domain::models::capability_token::CapabilityTokenId;
use crate::domain::models::orchestration_room::{HostBinding, ReviewVerdict};
use crate::domain::models::taint::ProvenanceTag;

/// Canonical SHA-256 digest used by all new durable orchestration content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn parse_hex(value: &str) -> Result<Self, ContentHashError> {
        if value.len() != 64 {
            return Err(ContentHashError::InvalidLength(value.len()));
        }
        if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(ContentHashError::NonCanonicalHex);
        }
        let mut bytes = [0u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = decode_hex_nibble(pair[0]).ok_or(ContentHashError::InvalidHex)?;
            let low = decode_hex_nibble(pair[1]).ok_or(ContentHashError::InvalidHex)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for ContentHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse_hex(&value).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContentHashError {
    #[error("content hash must contain 64 hexadecimal characters, got {0}")]
    InvalidLength(usize),
    #[error("content hash contains a non-hexadecimal character")]
    InvalidHex,
    #[error("content hash must use canonical lowercase hexadecimal")]
    NonCanonicalHex,
}

/// Artifact identity. Construction is restricted to a canonical content hash.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactId(String);

impl ArtifactId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn content_hash(&self) -> ContentHash {
        ContentHash::parse_hex(&self.0).expect("ArtifactId invariant: canonical content hash")
    }
}

impl From<ContentHash> for ArtifactId {
    fn from(content_hash: ContentHash) -> Self {
        Self(content_hash.to_string())
    }
}

impl fmt::Display for ArtifactId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ArtifactId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ArtifactId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let hash = ContentHash::parse_hex(&value).map_err(D::Error::custom)?;
        Ok(Self::from(hash))
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Evidence,
    Patch,
    TestResult,
    Decision,
    Review,
    /// 17.5b — an MCP task's elicitation request, filed to the scarce human
    /// as a ticket (FR152). The first artifact kind produced by an adapter.
    InputRequest,
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ReviewStatus {
    Pending,
    Reviewed {
        reviewer: AgentId,
        verdict: ReviewVerdict,
    },
}

/// Metadata supplied before the adapter computes the canonical content hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceArtifactDraft {
    pub kind: ArtifactKind,
    pub producer: AgentId,
    pub authority: CapabilityTokenId,
    pub provenance: Vec<ProvenanceTag>,
    pub depends_on: Vec<ArtifactId>,
    pub review: Option<ReviewStatus>,
    pub host: HostBinding,
}

/// Durable evidence metadata. The artifact body is never inlined here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceArtifact {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    pub producer: AgentId,
    pub content_hash: ContentHash,
    pub authority: CapabilityTokenId,
    pub provenance: Vec<ProvenanceTag>,
    pub depends_on: Vec<ArtifactId>,
    pub review: Option<ReviewStatus>,
    pub host: HostBinding,
}

pub type ArtifactRef = EvidenceArtifact;

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_id_is_derived_from_canonical_content_hash() {
        let hash = ContentHash::from_bytes([0xab; 32]);
        let id = ArtifactId::from(hash);
        assert_eq!(id.as_str(), "ab".repeat(32));
        assert_eq!(id.content_hash(), hash);
    }

    #[test]
    fn forged_artifact_id_fails_deserialization() {
        let error = serde_json::from_str::<ArtifactId>("\"not-a-hash\"")
            .expect_err("free-form artifact ids must be rejected");
        assert!(error.to_string().contains("64 hexadecimal"));
    }
}
