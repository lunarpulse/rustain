use std::ops::{Add, AddAssign, Sub};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
            budget: Budget {
                requests: 1,
                cost_micros: 1_000,
            },
            not_after: None,
            uses_limit: Some(1),
        }
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

    pub fn compute_id(&self) -> CapabilityTokenId {
        self.compute_id_with_signature_for_test(None)
    }

    /// Test seam proving signatures are excluded from token identity.
    pub fn compute_id_with_signature_for_test(
        &self,
        signature: Option<&[u8]>,
    ) -> CapabilityTokenId {
        let bytes = self.canonical_bytes(signature);
        let digest = Sha256::digest(&bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        CapabilityTokenId(out)
    }

    pub fn canonical_bytes(&self, signature: Option<&[u8]>) -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        out.extend_from_slice(DOMAIN_TAG);
        out.push(FORMAT_VERSION);
        push_opt_token_id(&mut out, self.parent);
        push_u64(&mut out, self.capabilities.0);
        push_str(&mut out, &self.scope.0);
        push_u64(&mut out, self.constraint.allowed.0);
        push_u64(&mut out, self.constraint.max_depth as u64);
        push_u64(&mut out, self.constraint.max_subset.0);
        push_u64(&mut out, self.budget.requests);
        push_u64(&mut out, self.budget.cost_micros);
        push_opt_u64(&mut out, self.not_after);
        push_opt_u32(&mut out, self.uses_limit);
        push_opt_str(&mut out, self.issuer.as_ref().map(|id| id.0.as_str()));
        // Signature is deliberately represented only by a constant exclusion tag:
        // id(None) == id(Some(fake64)) is the R1 proof required by ADR-14-2-02.
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
