//! `AuthorityTrust` — the independent trust the ledger's conservation recovery
//! relies on (Story Q-6 AC6; `DF-17-2d-AUTH-1`, narrow conservation scope).
//!
//! The room/transparency journal sits in the workspace and is only as trusted as
//! the filesystem. Recovery therefore never believes the journal on its own
//! word. It needs two things the journal cannot supply:
//!
//! - **authenticity** — every conservation head carries a signature made by a
//!   local authority key held *outside the workspace*; a record the workspace
//!   journal merely asserts fails verification;
//! - **freshness** — an *anchor* (the highest conservation sequence and
//!   authority time ever flushed) stored beside the key, so a journal that was
//!   truncated or rolled back to an older, valid prefix is detected rather than
//!   admitted.
//!
//! Authority lifecycle (approved narrow design): an authority is *established*
//! the first time a workspace with **no** conservation history starts and no
//! anchor exists for it; establishing writes the anchor. From then on a restart
//! is admitted only from a verified, fresh history. Missing history alone is
//! never evidence of first initialization.
//!
//! The port is synchronous and domain-only; the filesystem implementation is
//! `infrastructure::subagent::authority_trust`.

/// The independently stored freshness watermark.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuthorityAnchor {
    /// Highest `ledger_seq` whose record is known durable.
    pub ledger_seq: u64,
    /// Authority-time high-water mark at that sequence.
    pub authority_time_ms: u64,
}

/// A trust-store failure. The text is a safe local diagnostic: no key material,
/// no path of a secret, no record content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityTrustError(pub String);

impl std::fmt::Display for AuthorityTrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "authority trust store: {}", self.0)
    }
}

impl std::error::Error for AuthorityTrustError {}

/// Signs and verifies conservation heads and holds the freshness anchor.
pub trait AuthorityTrust: Send + Sync {
    /// Sign `message` with the local authority key (bound to this workspace).
    fn sign(&self, message: &[u8]) -> Vec<u8>;

    /// Whether `signature` is this authority's signature over `message`.
    fn verify(&self, message: &[u8], signature: &[u8]) -> bool;

    /// The verified anchor loaded when the trust was opened or last advanced.
    fn anchor(&self) -> AuthorityAnchor;

    /// Durably raise the anchor. Never lowers it; an equal or lower value is a
    /// successful no-op. Written only AFTER the records it covers are durable, so
    /// the anchor is always `<=` the journal (crash-consistent ordering).
    fn advance_anchor(&self, to: AuthorityAnchor) -> Result<(), AuthorityTrustError>;
}
