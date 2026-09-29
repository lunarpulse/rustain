//! AgentCard EdDSA JWS verification.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::adapters::rap::AgentSigner;
use crate::domain::models::{PinnedKey, PinnedKeyAlgorithm};

use super::card::ServedAgentCard;
use super::error::A2aError;

#[derive(Deserialize)]
struct SignatureEntry {
    protected: String,
    signature: String,
}

#[derive(Deserialize, Serialize)]
struct ProtectedHeader {
    alg: String,
    kid: Option<String>,
}

pub fn sign_card(card: &ServedAgentCard, signer: &AgentSigner) -> Result<String, A2aError> {
    let kid = signer.identity().peer_id.to_string();
    let protected_bytes = serde_jcs::to_vec(&ProtectedHeader {
        alg: "EdDSA".to_owned(),
        kid: Some(kid),
    })
    .map_err(|error| A2aError::Canonicalization(error.to_string()))?;
    let protected = URL_SAFE_NO_PAD.encode(protected_bytes);
    let payload =
        serde_jcs::to_vec(card).map_err(|error| A2aError::Canonicalization(error.to_string()))?;
    let payload = URL_SAFE_NO_PAD.encode(payload);
    let mut signing_input = Vec::with_capacity(protected.len() + 1 + payload.len());
    signing_input.extend_from_slice(protected.as_bytes());
    signing_input.push(b'.');
    signing_input.extend_from_slice(payload.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(signer.sign_detached(&signing_input).as_bytes());
    let signed = card.clone().with_signature(protected, signature);
    serde_jcs::to_string(&signed).map_err(|error| A2aError::Canonicalization(error.to_string()))
}

pub(crate) fn decode_verifying_key(pinned: &PinnedKey) -> Result<VerifyingKey, A2aError> {
    if !matches!(pinned.alg, PinnedKeyAlgorithm::EdDsa) {
        return Err(A2aError::UnsupportedAlgorithm {
            algorithm: format!("{:?}", pinned.alg),
        });
    }
    let key_bytes = URL_SAFE_NO_PAD
        .decode(pinned.x.as_bytes())
        .map_err(|_| A2aError::InvalidPinnedKey)?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| A2aError::InvalidPinnedKey)?;
    VerifyingKey::from_bytes(&key_bytes).map_err(|_| A2aError::InvalidPinnedKey)
}

pub fn verify_card(raw_bytes: &str, pinned: &PinnedKey) -> Result<(), A2aError> {
    let verifying_key = decode_verifying_key(pinned)?;

    let mut card: serde_json::Value = serde_json::from_str(raw_bytes)?;
    let signatures = card
        .as_object_mut()
        .and_then(|object| object.remove("signatures"))
        .and_then(|value| value.as_array().cloned())
        .filter(|entries| !entries.is_empty())
        .ok_or(A2aError::MissingSignatures)?;

    let payload =
        serde_jcs::to_vec(&card).map_err(|error| A2aError::Canonicalization(error.to_string()))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload);
    let mut last_error = A2aError::BadSignature;

    for entry_value in signatures {
        let entry: SignatureEntry = match serde_json::from_value(entry_value) {
            Ok(entry) => entry,
            Err(_) => {
                last_error = A2aError::InvalidSignatureEncoding;
                continue;
            }
        };
        let protected_bytes = match URL_SAFE_NO_PAD.decode(entry.protected.as_bytes()) {
            Ok(bytes) => bytes,
            Err(_) => {
                last_error = A2aError::InvalidProtectedHeader;
                continue;
            }
        };
        let header: ProtectedHeader = match serde_json::from_slice(&protected_bytes) {
            Ok(header) => header,
            Err(_) => {
                last_error = A2aError::InvalidProtectedHeader;
                continue;
            }
        };
        if header.alg != "EdDSA" {
            last_error = A2aError::UnsupportedAlgorithm {
                algorithm: header.alg,
            };
            continue;
        }
        let Some(header_kid) = header.kid else {
            last_error = A2aError::InvalidProtectedHeader;
            continue;
        };
        if let Some(expected) = &pinned.kid
            && &header_kid != expected
        {
            last_error = A2aError::KeyIdMismatch {
                expected: expected.clone(),
                actual: Some(header_kid),
            };
            continue;
        }

        let signature_bytes = match URL_SAFE_NO_PAD.decode(entry.signature.as_bytes()) {
            Ok(bytes) => bytes,
            Err(_) => {
                last_error = A2aError::InvalidSignatureEncoding;
                continue;
            }
        };
        let signature_bytes: [u8; 64] = match signature_bytes.try_into() {
            Ok(bytes) => bytes,
            Err(_) => {
                last_error = A2aError::InvalidSignatureEncoding;
                continue;
            }
        };
        let signature = Signature::from_bytes(&signature_bytes);
        let signing_input = format!("{}.{}", entry.protected, payload_b64);
        if verifying_key
            .verify_strict(signing_input.as_bytes(), &signature)
            .is_ok()
        {
            return Ok(());
        }
        last_error = A2aError::BadSignature;
    }

    Err(last_error)
}
