use std::ops::{Add, AddAssign, Sub};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::domain::models::agent_id::AgentId;
use crate::domain::models::peer_identity::{Ed25519Sig, PeerId};

const DOMAIN_TAG: &[u8] = b"rustain.captoken.";
const FORMAT_VERSION: u8 = 1;

/// Stable identifier for a capability token.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct CapabilityTokenId(pub [u8; 32]);

impl CapabilityTokenId {
    /// Fixture sentinel for code paths that must construct nodes before Story
    /// 14.2 authority is wired. Not a minted authority grant.
    pub const fn nil() -> Self {
        Self([0; 32])
    }

    /// Root fixture sentinel. Real root tokens are minted with [`CapabilityToken::root`].
    pub const fn root() -> Self {
        let mut bytes = [0; 32];
        bytes[0] = 1;
        Self(bytes)
    }
}

/// Authority capability flags. Distinct from discovery-side `Capability`.
#[non_exhaustive]
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CapabilityFlag {
    Spawn = 1,
    ReadFs = 2,
    WriteFs = 3,
    Network = 4,
}

impl CapabilityFlag {
    pub const fn bit(self) -> u64 {
        1u64 << (self as u8)
    }
}

/// O(1) deterministic bitset over [`CapabilityFlag`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CapabilitySet(pub u64);

impl CapabilitySet {
    pub const EMPTY: Self = Self(0);

    pub fn from_flags(flags: &[CapabilityFlag]) -> Self {
        let mut bits = 0u64;
        for flag in flags {
            bits |= flag.bit();
        }
        Self(bits)
    }

    pub const fn contains(self, flag: CapabilityFlag) -> bool {
        (self.0 & flag.bit()) != 0
    }

    pub const fn is_subset_of(self, parent: Self) -> bool {
        (self.0 & parent.0) == self.0
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

/// Request/cost budget, represented as integers for canonical bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Budget {
    pub requests: u64,
    pub cost_micros: u64,
}

/// Budget reserved by one fork-join child delegation.
pub const R1_CHILD_BUDGET: Budget = Budget {
    requests: 1,
    cost_micros: 1_000,
};

/// Budget reserved by one fork-join spawn gate.
pub const R1_GATE_TOKEN_BUDGET: Budget = Budget {
    requests: 1,
    cost_micros: 1,
};

/// Budget consumed by the coordinator's deterministic synthesis floor.
pub const R1_SYNTHESIS_RESERVE: Budget = Budget {
    requests: 1,
    cost_micros: 1_000,
};

impl Budget {
    pub const ZERO: Self = Self {
        requests: 0,
        cost_micros: 0,
    };

    pub const fn is_within(self, parent: Self) -> bool {
        self.requests <= parent.requests && self.cost_micros <= parent.cost_micros
    }

    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self {
            requests: self.requests.saturating_sub(rhs.requests),
            cost_micros: self.cost_micros.saturating_sub(rhs.cost_micros),
        }
    }
}

impl Add for Budget {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self {
            requests: self.requests + rhs.requests,
            cost_micros: self.cost_micros + rhs.cost_micros,
        }
    }
}

impl AddAssign for Budget {
    fn add_assign(&mut self, rhs: Self) {
        self.requests += rhs.requests;
        self.cost_micros += rhs.cost_micros;
    }
}

impl Sub for Budget {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            requests: self.requests - rhs.requests,
            cost_micros: self.cost_micros - rhs.cost_micros,
        }
    }
}

/// Child delegation ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateConstraint {
    pub allowed: CapabilitySet,
    pub max_depth: usize,
    pub max_subset: CapabilitySet,
}

/// Input to `delegate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateRequest {
    pub scope: AgentId,
    pub capabilities: CapabilitySet,
    pub constraint: DelegateConstraint,
    pub budget: Budget,
    pub not_after: Option<u64>,
    pub uses_limit: Option<u32>,
}

/// Immutable authority grant. Mutable consumption lives in `AuthorityLedger`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityToken {
    pub id: CapabilityTokenId,
    pub parent: Option<CapabilityTokenId>,
    pub capabilities: CapabilitySet,
    pub scope: AgentId,
    pub constraint: DelegateConstraint,
    pub budget: Budget,
    pub not_after: Option<u64>,
    pub uses_limit: Option<u32>,
    pub issuer: Option<PeerId>,
    pub signature: Option<Ed25519Sig>,
}

impl CapabilityToken {
    pub fn root(
        scope: AgentId,
        capabilities: CapabilitySet,
        budget: Budget,
        max_depth: usize,
        not_after: Option<u64>,
        uses_limit: Option<u32>,
    ) -> Self {
        let mut token = Self {
            id: CapabilityTokenId::nil(),
            parent: None,
            capabilities,
            scope,
            constraint: DelegateConstraint {
                allowed: capabilities,
                max_depth,
                max_subset: capabilities,
            },
            budget,
            not_after,
            uses_limit,
            issuer: None,
            signature: None,
        };
        token.id = token.compute_id();
        token
    }

    pub fn r1_root(scope: AgentId) -> Self {
        Self::root(
            scope,
            CapabilitySet::from_flags(&[
                CapabilityFlag::Spawn,
                CapabilityFlag::ReadFs,
                CapabilityFlag::WriteFs,
                CapabilityFlag::Network,
            ]),
            Budget {
                requests: 1_000,
                cost_micros: 1_000_000_000,
            },
            3,
            None,
            Some(1_000),
        )
    }

    pub fn r1_child_request(scope: AgentId) -> DelegateRequest {
        let capabilities = CapabilitySet::from_flags(&[
            CapabilityFlag::Spawn,
            CapabilityFlag::ReadFs,
            CapabilityFlag::WriteFs,
            CapabilityFlag::Network,
        ]);
        DelegateRequest {
            scope,
            capabilities,
            constraint: DelegateConstraint {
                allowed: capabilities,
                max_depth: 3,
                max_subset: capabilities,
            },
            budget: R1_CHILD_BUDGET,
            not_after: None,
            uses_limit: Some(1),
        }
    }

    /// Build the exact authority request for a declarative nested coordinator.
    ///
    /// The coordinator must fund each grandchild's delegation and spawn gate,
    /// plus one deterministic synthesis reservation. No arbitrary padding is
    /// added: changing `grandchild_count` changes the grant by exactly one
    /// child budget plus one gate budget.
    pub fn r1_coordinator(scope: AgentId, grandchild_count: usize) -> DelegateRequest {
        let mut request = Self::r1_child_request(scope);
        let count = grandchild_count as u64;
        request.budget = Budget {
            requests: (R1_CHILD_BUDGET.requests + R1_GATE_TOKEN_BUDGET.requests) * count
                + R1_SYNTHESIS_RESERVE.requests,
            cost_micros: (R1_CHILD_BUDGET.cost_micros + R1_GATE_TOKEN_BUDGET.cost_micros) * count
                + R1_SYNTHESIS_RESERVE.cost_micros,
        };
        request
    }

    pub fn child(parent: &Self, req: DelegateRequest) -> Self {
        let mut token = Self {
            id: CapabilityTokenId::nil(),
            parent: Some(parent.id),
            capabilities: req.capabilities,
            scope: req.scope,
            constraint: req.constraint,
            budget: req.budget,
            not_after: req.not_after,
            uses_limit: req.uses_limit,
            issuer: None,
            signature: None,
        };
        token.id = token.compute_id();
        token
    }

    /// Stable token identity. Issuer and signature are authenticated transport
    /// attestations, not authority content, so attaching either cannot change
    /// the id used by parent references and ledger entries.
    pub fn compute_id(&self) -> CapabilityTokenId {
        self.compute_id_with_signature_for_test(None)
    }

    /// Test seam proving signatures are excluded from token identity.
    pub fn compute_id_with_signature_for_test(
        &self,
        signature: Option<&[u8]>,
    ) -> CapabilityTokenId {
        let _ = signature;
        let digest = Sha256::digest(self.identity_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        CapabilityTokenId(out)
    }

    fn identity_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        out.extend_from_slice(DOMAIN_TAG);
        out.push(FORMAT_VERSION);
        push_opt_token_id(&mut out, self.parent);
        push_u64(&mut out, self.capabilities.0);
        push_str(&mut out, self.scope.as_str());
        push_u64(&mut out, self.constraint.allowed.0);
        push_u64(&mut out, self.constraint.max_depth as u64);
        push_u64(&mut out, self.constraint.max_subset.0);
        push_u64(&mut out, self.budget.requests);
        push_u64(&mut out, self.budget.cost_micros);
        push_opt_u64(&mut out, self.not_after);
        push_opt_u32(&mut out, self.uses_limit);
        out
    }

    /// Canonical bytes signed by the issuer. The issuer is included so the
    /// signature binds the claimed key; the signature itself is excluded.
    pub fn canonical_bytes(&self, signature: Option<&[u8]>) -> Vec<u8> {
        let mut out = self.identity_bytes();
        push_opt_str(&mut out, self.issuer.as_ref().map(|id| id.as_str()));
        let _ = signature;
        out.push(0xFE);
        out
    }

    pub fn malformed_signature_state(&self) -> bool {
        self.issuer.is_some() != self.signature.is_some()
    }

    pub fn has_signature(&self) -> bool {
        self.signature.is_some()
    }

    /// Cryptographically sign this token with the issuer's Ed25519 key.
    ///
    /// Sets `issuer` and `signature` together (preserving the both-or-neither
    /// invariant enforced by [`Self::malformed_signature_state`]). The signature
    /// covers [`Self::canonical_bytes`] with the signature excluded, so the id
    /// (also derived from those canonical bytes) is preserved across signature
    /// presence: `id(with-sig) == id(without-sig)`.
    pub fn sign(&mut self, signing_key: &SigningKey, issuer: PeerId) {
        self.issuer = Some(issuer);
        // Recompute the id with the issuer now bound; the signature is excluded
        // from the canonical bytes so the id is stable once set.
        self.signature = None;
        self.id = self.compute_id();
        let signing_input = self.canonical_bytes(None);
        let sig = signing_key.sign(&signing_input);
        self.signature = Some(Ed25519Sig(sig.to_bytes().to_vec()));
    }

    /// Verify the issuer signature and content integrity cryptographically.
    ///
    /// Gates (all must pass): (1) issuer and signature are both present;
    /// (2) the issuer [`PeerId`] matches the verifying key; (3) the Ed25519
    /// signature is valid over the canonical bytes; (4) the stored id equals
    /// the recomputed content hash (defense-in-depth tamper detection).
    pub fn verify(&self, verifying_key: &VerifyingKey) -> Result<(), CapabilityTokenError> {
        let sig = self
            .signature
            .as_ref()
            .ok_or(CapabilityTokenError::NotSigned)?;
        let issuer = self
            .issuer
            .as_ref()
            .ok_or(CapabilityTokenError::NotSigned)?;
        if !issuer.matches_public_key(&verifying_key.to_bytes()) {
            return Err(CapabilityTokenError::IssuerKeyMismatch);
        }
        let signing_input = self.canonical_bytes(None);
        let sig_bytes: [u8; 64] = sig
            .as_bytes()
            .try_into()
            .map_err(|_| CapabilityTokenError::InvalidSignatureLength(sig.as_bytes().len()))?;
        verifying_key
            .verify(&signing_input, &Signature::from_bytes(&sig_bytes))
            .map_err(|_| CapabilityTokenError::BadSignature)?;
        if !self.integrity_ok() {
            return Err(CapabilityTokenError::IntegrityFailed);
        }
        Ok(())
    }

    /// Self-consistency check: the stored id equals the content hash recomputed
    /// from the canonical bytes. The authority ledger uses this to gate signed
    /// tokens without a verifying key; a tampered field changes the content
    /// hash and fails here.
    pub fn integrity_ok(&self) -> bool {
        self.id == self.compute_id()
    }

    pub fn is_subset_of(&self, parent: &Self) -> bool {
        self.capabilities.is_subset_of(parent.capabilities)
            && self.capabilities.is_subset_of(parent.constraint.allowed)
            && self.capabilities.is_subset_of(parent.constraint.max_subset)
            && self
                .constraint
                .allowed
                .is_subset_of(parent.constraint.allowed)
            && self
                .constraint
                .max_subset
                .is_subset_of(parent.constraint.max_subset)
            && self.budget.is_within(parent.budget)
            && option_u64_lte(self.not_after, parent.not_after)
            && option_u32_lte(self.uses_limit, parent.uses_limit)
            && self.constraint.max_depth <= parent.constraint.max_depth
    }
}

/// Failure modes for [`CapabilityToken::verify`] (cryptographic signed-token
/// verification).
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CapabilityTokenError {
    #[error("capability token is not signed (issuer and signature both absent)")]
    NotSigned,
    #[error("capability token issuer peer id does not match the verifying key")]
    IssuerKeyMismatch,
    #[error("capability token signature is not a valid 64-byte ed25519 signature")]
    InvalidSignatureLength(usize),
    #[error("capability token signature is invalid")]
    BadSignature,
    #[error("capability token id does not match recomputed content (integrity failed)")]
    IntegrityFailed,
}

fn option_u64_lte(child: Option<u64>, parent: Option<u64>) -> bool {
    match (child, parent) {
        (_, None) => true,
        (Some(c), Some(p)) => c <= p,
        (None, Some(_)) => false,
    }
}

fn option_u32_lte(child: Option<u32>, parent: Option<u32>) -> bool {
    match (child, parent) {
        (_, None) => true,
        (Some(c), Some(p)) => c <= p,
        (None, Some(_)) => false,
    }
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_len(out: &mut Vec<u8>, len: usize) {
    push_u64(out, len as u64);
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn push_str(out: &mut Vec<u8>, value: &str) {
    push_bytes(out, value.as_bytes());
}

fn push_opt_str(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(s) => {
            out.push(1);
            push_str(out, s);
        }
        None => out.push(0),
    }
}

fn push_opt_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(v) => {
            out.push(1);
            push_u64(out, v);
        }
        None => out.push(0),
    }
}

fn push_opt_u32(out: &mut Vec<u8>, value: Option<u32>) {
    match value {
        Some(v) => {
            out.push(1);
            push_u32(out, v);
        }
        None => out.push(0),
    }
}

fn push_opt_token_id(out: &mut Vec<u8>, value: Option<CapabilityTokenId>) {
    match value {
        Some(v) => {
            out.push(1);
            out.extend_from_slice(&v.0);
        }
        None => out.push(0),
    }
}

#[cfg(test)]
mod sign_verify_tests {
    use super::*;
    use crate::domain::models::agent_id::AgentId;

    fn issuer_key() -> (SigningKey, VerifyingKey, PeerId) {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let vk = key.verifying_key();
        let peer_id = PeerId::from_public_key(&vk.to_bytes()).unwrap();
        (key, vk, peer_id)
    }

    fn signed_token() -> (CapabilityToken, VerifyingKey) {
        let scope = AgentId::parse("peer/agent").unwrap();
        let mut token = CapabilityToken::r1_root(scope);
        let (key, vk, peer_id) = issuer_key();
        token.sign(&key, peer_id);
        (token, vk)
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let (token, vk) = signed_token();
        assert!(token.has_signature());
        assert!(!token.malformed_signature_state());
        token.verify(&vk).expect("signed token must verify");
    }

    #[test]
    fn id_is_preserved_under_signature_presence() {
        // The id is derived from canonical bytes that exclude the signature, so
        // attaching a signature must not change the id (ADR-14-2-02).
        let scope = AgentId::parse("peer/agent").unwrap();
        let mut token = CapabilityToken::r1_root(scope);
        let (key, _vk, peer_id) = issuer_key();
        // Set the issuer first so both states share identical non-signature fields.
        token.issuer = Some(peer_id.clone());
        let id_before = token.compute_id();
        token.sign(&key, peer_id);
        assert_eq!(
            token.id, id_before,
            "id must be stable across signature presence"
        );
        assert_eq!(token.compute_id(), id_before);
    }

    #[test]
    fn signing_fresh_token_never_changes_its_identity() {
        let mut token = CapabilityToken::r1_root(AgentId::parse("peer/agent").unwrap());
        let id_before = token.id;
        let (key, _vk, peer_id) = issuer_key();
        token.sign(&key, peer_id);
        assert_eq!(token.id, id_before);
    }

    #[test]
    fn verify_rejects_tampered_field() {
        let (mut token, vk) = signed_token();
        // Tamper with a covered field after signing — signature + integrity fail.
        token.budget.requests += 1;
        assert!(matches!(
            token.verify(&vk),
            Err(CapabilityTokenError::BadSignature)
        ));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let (token, _vk) = signed_token();
        let other = SigningKey::from_bytes(&[99u8; 32]).verifying_key();
        assert!(matches!(
            token.verify(&other),
            Err(CapabilityTokenError::IssuerKeyMismatch)
        ));
    }

    #[test]
    fn verify_rejects_unsigned_and_issuer_key_mismatch() {
        let scope = AgentId::parse("peer/agent").unwrap();
        let token = CapabilityToken::r1_root(scope); // unsigned
        let other_vk = SigningKey::from_bytes(&[99u8; 32]).verifying_key();
        assert!(matches!(
            token.verify(&other_vk),
            Err(CapabilityTokenError::NotSigned)
        ));

        // Sign with one key but lie about the issuer (different PeerId).
        let (key, _vk, _real_peer) = issuer_key();
        let mut bad = CapabilityToken::r1_root(AgentId::parse("peer/x").unwrap());
        let fake_peer = PeerId::from_public_key(&[3u8; 32]).unwrap();
        bad.sign(&key, fake_peer);
        // Verifying against the real key: issuer PeerId won't match the key.
        let real_vk = key.verifying_key();
        assert!(matches!(
            bad.verify(&real_vk),
            Err(CapabilityTokenError::IssuerKeyMismatch)
        ));
    }

    #[test]
    fn coordinator_budget_is_exactly_derived_from_grandchild_count() {
        let request = CapabilityToken::r1_coordinator(AgentId::parse("coordinator").unwrap(), 3);
        assert_eq!(
            request.budget,
            Budget {
                requests: 7,
                cost_micros: 4_003,
            }
        );

        let empty =
            CapabilityToken::r1_coordinator(AgentId::parse("empty-coordinator").unwrap(), 0);
        assert_eq!(empty.budget, R1_SYNTHESIS_RESERVE);
    }
}
