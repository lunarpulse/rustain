//! Recipient-side durable-item identity.
//!
//! The address retains principal provenance. AD-1821's stronger transport-
//! authenticated identity is represented by [`ItemPrincipal::Rap`]; the A2A
//! ingress in Story 19.16 is legal only through AD-1825's pseudonymous
//! provenance narrowing.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::PeerId;

/// Recipient-minted opaque item identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(String);

impl ItemId {
    /// Reconstruct an identifier already present in the durable journal.
    #[must_use]
    pub fn from_replay(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Principal provenance carried as part of every durable-item address.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "peer")]
pub enum ItemPrincipal {
    /// Identity supplied by the authenticated RAP ingress.
    Rap(PeerId),
    /// Stable pseudonym derived from the credential accepted by A2A ingress.
    A2aPseudonym(PeerId),
    /// A provenance written by a newer build.
    #[serde(other)]
    Unknown,
}

/// Complete recipient-side address.
///
/// Item-id uniqueness is **global, not per principal**: [`RecipientItemAllocator`]
/// keys `claimed_ids` on the bare id and refuses one already claimed under any
/// principal, because `/team ack` and `/team remove` address an item by id
/// alone. AD-1825's per-`(provenance, PeerId)` rule is the floor this exceeds,
/// never a cap.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemAddress {
    principal: ItemPrincipal,
    item: ItemId,
}

impl ItemAddress {
    /// Construct only at the A2A ingress after credential handling derived the
    /// supplied pseudonym. There is intentionally no `From<PeerId>` conversion.
    #[must_use]
    pub fn from_a2a_ingress(peer: PeerId, item: ItemId) -> Self {
        Self {
            principal: ItemPrincipal::A2aPseudonym(peer),
            item,
        }
    }

    /// Construct only at the RAP ingress after envelope verification supplied
    /// the transport identity. There is intentionally no `From<PeerId>` conversion.
    #[must_use]
    pub fn from_rap_ingress(peer: PeerId, item: ItemId) -> Self {
        Self {
            principal: ItemPrincipal::Rap(peer),
            item,
        }
    }

    #[must_use]
    pub fn principal(&self) -> &ItemPrincipal {
        &self.principal
    }

    #[must_use]
    pub fn item(&self) -> &ItemId {
        &self.item
    }
}

/// Current replay-folded recipient-item state.
///
/// `#[non_exhaustive]` per AC4(d)/A15 (NFR68): downstream consumers must
/// already tolerate a state they do not know. The one exhaustive `match` is
/// the legality table in `next_item_state` over `(state, act)`
/// (`adapters/policy/recipient_item.rs`), same crate.
///
/// The payload lives **inside** the state (19.16b review, paying
/// `DF-19-16C-ITEM-VIEW-FIELD-COUNT`'s prescribed fix): `content` moves in,
/// which makes the old `content: None ⟺ Removed` invariant unrepresentable
/// instead of asserted, and `Removed` carries the **pre-removal
/// acknowledgement** so the board can render a removed-after-ack item at its
/// last sender-visible outcome (`A22`) without a second writer of that fact
/// beside the state (`AD-1827`). The cost, recorded in the DF, is `Copy`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipientItemState {
    Received {
        content: String,
    },
    Acknowledged {
        content: String,
    },
    /// The recipient disposed of their own copy (FR165). **Terminal**: no act
    /// leaves it, from either predecessor. The entry itself is **kept**,
    /// because an absent entry is byte-identically "not found" and AD-1822
    /// requires a tombstone distinct from one. `acknowledged_before` records
    /// which predecessor the tombstone came from — the one fact
    /// `DF-19-16B-TOMBSTONE-LOSES-ACK-PREDECESSOR` needed the read to keep.
    Removed {
        acknowledged_before: bool,
    },
}

impl RecipientItemState {
    /// The stable wire tag (`AC1(e)` pins `state ∈ {received, acknowledged,
    /// removed}` as a bare string). The payload never reaches the wire through
    /// this name; it is projected field-by-field by the serving verb.
    #[must_use]
    pub const fn wire_name(&self) -> &'static str {
        match self {
            Self::Received { .. } => "received",
            Self::Acknowledged { .. } => "acknowledged",
            Self::Removed { .. } => "removed",
        }
    }

    /// `Some(true)` exactly when this tombstone came from `Acknowledged` —
    /// the predecessor fact the board renders (`A22`). `None` while live.
    #[must_use]
    pub const fn acknowledged_before_removal(&self) -> Option<bool> {
        match self {
            Self::Removed {
                acknowledged_before,
            } => Some(*acknowledged_before),
            Self::Received { .. } | Self::Acknowledged { .. } => None,
        }
    }
}

/// Content and state reconstructed solely from durable recipient-item events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipientItemView {
    pub address: ItemAddress,
    pub task: String,
    pub alias: Option<String>,
    /// The content lives **inside** [`RecipientItemState`]
    /// (`DF-19-16C-ITEM-VIEW-FIELD-COUNT`'s fix): a removal replaces the
    /// variant and the content goes with it — unrepresentable, not asserted.
    pub state: RecipientItemState,
    /// Fold order of the creating `RecipientItemReceived` event. An ordering
    /// aid for ambiguous task lookups (a resent `messageId` produces one item
    /// per execution); never part of the address.
    ///
    /// ⚠ Deterministic **re-fold versus re-fold**, which is all AC4's
    /// structural-equality ratchet compares. It is NOT stable across the two
    /// assignment schemes: `from_entries` uses the absolute journal index and
    /// `apply` uses `max(existing) + 1`, so a cross-scheme comparison must
    /// exclude this field. Both are monotonic in append order, so every
    /// ordering consumer agrees regardless.
    pub journal_order: u64,
}

/// A minted id collided with an address already present in durable history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemIdCollision;

impl std::fmt::Display for ItemIdCollision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("recipient item id collision")
    }
}

impl std::error::Error for ItemIdCollision {}

/// Race-free recipient-side id allocator seeded from the complete durable fold.
///
/// A collision is refused rather than silently re-minted: callers must never
/// receive a different id than the one paired with their durable event. Ids are
/// unique across the whole durable history GLOBALLY, not merely per principal
/// (AC1(c2)) — `/team ack <item-id>` addresses an item by id alone (AC2), so a
/// cross-principal duplicate would make both items unacknowledgeable.
pub struct RecipientItemAllocator {
    claimed_ids: tokio::sync::Mutex<HashSet<String>>,
}

impl RecipientItemAllocator {
    #[must_use]
    pub fn from_addresses(addresses: impl IntoIterator<Item = ItemAddress>) -> Self {
        Self {
            claimed_ids: tokio::sync::Mutex::new(
                addresses
                    .into_iter()
                    .map(|address| address.item().as_str().to_owned())
                    .collect(),
            ),
        }
    }

    /// Mint an opaque id independent of every sender-controlled field.
    pub async fn allocate_a2a_ingress(&self, peer: PeerId) -> Result<ItemAddress, ItemIdCollision> {
        self.claim_a2a_candidate(peer, ItemId(format!("ri_{}", nanoid::nanoid!(32))))
            .await
    }

    async fn claim_a2a_candidate(
        &self,
        peer: PeerId,
        item: ItemId,
    ) -> Result<ItemAddress, ItemIdCollision> {
        let id = item.as_str().to_owned();
        let address = ItemAddress::from_a2a_ingress(peer, item);
        // One lock, one check: the id is the acknowledge command's only handle,
        // so it must be unique even across principals.
        if !self.claimed_ids.lock().await.insert(id) {
            return Err(ItemIdCollision);
        }
        Ok(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_history_collision_is_refused_without_reminting() {
        let peer = PeerId::from_public_key(&[4; 32]).unwrap();
        let existing = ItemAddress::from_a2a_ingress(peer.clone(), ItemId::from_replay("ri_fixed"));
        let allocator = RecipientItemAllocator::from_addresses([existing]);

        assert_eq!(
            allocator
                .claim_a2a_candidate(peer, ItemId::from_replay("ri_fixed"))
                .await,
            Err(ItemIdCollision)
        );
    }

    #[tokio::test]
    async fn a_cross_principal_id_collision_is_refused_without_reminting() {
        // `/team ack <item-id>` carries no principal, so an id already claimed
        // under one principal must be refused under every other.
        let first_peer = PeerId::from_public_key(&[4; 32]).unwrap();
        let second_peer = PeerId::from_public_key(&[5; 32]).unwrap();
        let existing = ItemAddress::from_a2a_ingress(first_peer, ItemId::from_replay("ri_fixed"));
        let allocator = RecipientItemAllocator::from_addresses([existing]);

        assert_eq!(
            allocator
                .claim_a2a_candidate(second_peer, ItemId::from_replay("ri_fixed"))
                .await,
            Err(ItemIdCollision)
        );
    }
}
