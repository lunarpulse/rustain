//! Recipient-side durable-item identity.
//!
//! The address retains principal provenance. AD-1821's stronger transport-
//! authenticated identity is represented by [`ItemPrincipal::Rap`]; the A2A
//! ingress in Story 19.16 is legal only through AD-1825's pseudonymous
//! provenance narrowing; Story 19.18d adds the operator-bound pin
//! ([`ItemPrincipal::A2aBound`]), a third provenance kept apart from both.
use super::provenance::Authorship;

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};
use serde::{Deserializer, Serializer};
use std::collections::HashSet;
use std::fmt;

use super::{AgentId, PeerId};

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
///
/// Story 19.18d `A4`/`OQ2` — persisted only through [`ItemAddress`] and
/// [`TurnAddress`], whose wire form carries a bound pin in a sibling
/// `boundPeer` field so an older build reads `{"kind":"a2a_bound"}` as
/// [`Self::Unknown`] instead of failing the whole journal. `A2aBound` is
/// therefore skipped by this type's own serde: it has no direct encoding.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "peer")]
pub enum ItemPrincipal {
    /// Identity supplied by the authenticated RAP ingress.
    Rap(PeerId),
    /// Stable pseudonym derived from the credential accepted by A2A ingress.
    A2aPseudonym(PeerId),
    /// Story 19.18d — the pinned roster peer an operator bound the presented
    /// A2A key to: an operator assertion of key possession, never the person.
    #[serde(skip)]
    A2aBound(PeerId),
    /// A provenance written by a newer build.
    #[serde(other)]
    Unknown,
}

/// Story 19.18d `A6` — the only principals A2A ingress can mint items under.
/// It names no RAP variant, so the A2A allocator cannot construct `Rap`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum A2aIngressPrincipal {
    /// The pseudonym of the presented credential (unbound key or loopback).
    Pseudonym(PeerId),
    /// The pin an operator bound the presented key to.
    Bound(PeerId),
}

/// Story 19.18d `A11` — the item principals one caller may read: its own,
/// plus, for a bound caller, the pseudonyms of keys bound to the same pin
/// with `adoptPreBinding`. Deduplicated; membership is the projection's one
/// read predicate. Minting stays on the exact principal; `tasks/get` and
/// `tasks/cancel` of a bound caller also resolve its adopted keys' recovered
/// terminal tasks (Story 19.18e).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemScope {
    members: Vec<ItemPrincipal>,
}

impl ItemScope {
    /// A scope of exactly one principal.
    #[must_use]
    pub fn exact(principal: ItemPrincipal) -> Self {
        Self {
            members: vec![principal],
        }
    }

    /// `principal` plus every adopted principal, without duplicates.
    #[must_use]
    pub fn with_adopted(
        principal: ItemPrincipal,
        adopted: impl IntoIterator<Item = ItemPrincipal>,
    ) -> Self {
        let mut members = vec![principal];
        for member in adopted {
            if !members.contains(&member) {
                members.push(member);
            }
        }
        Self { members }
    }

    /// Whether an item or turn under `principal` is readable in this scope.
    #[must_use]
    pub fn contains(&self, principal: &ItemPrincipal) -> bool {
        self.members.contains(principal)
    }
}

/// Story 19.18d `A4` — the on-disk principal of an address. Identical to
/// [`ItemPrincipal`]'s encoding for every pre-19.18d kind; the bound kind is
/// the unit `{"kind":"a2a_bound"}`, its pin in the address's `boundPeer`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "peer")]
enum WirePrincipal {
    Rap(PeerId),
    A2aPseudonym(PeerId),
    A2aBound,
    #[serde(other)]
    Unknown,
}

impl WirePrincipal {
    fn split(principal: &ItemPrincipal) -> (Self, Option<String>) {
        match principal {
            ItemPrincipal::Rap(peer) => (Self::Rap(peer.clone()), None),
            ItemPrincipal::A2aPseudonym(peer) => (Self::A2aPseudonym(peer.clone()), None),
            ItemPrincipal::A2aBound(pin) => (Self::A2aBound, Some(pin.as_str().to_owned())),
            ItemPrincipal::Unknown => (Self::Unknown, None),
        }
    }

    /// `boundPeer` is read as text and parsed here (`F10`): a malformed pin
    /// folds this one address to `Unknown` — fail closed — instead of failing
    /// the whole journal. A stray `boundPeer` on another kind is ignored.
    fn join(self, bound_peer: Option<String>) -> ItemPrincipal {
        match self {
            Self::Rap(peer) => ItemPrincipal::Rap(peer),
            Self::A2aPseudonym(peer) => ItemPrincipal::A2aPseudonym(peer),
            Self::A2aBound => bound_peer
                .and_then(|pin| PeerId::parse(pin).ok())
                .map_or(ItemPrincipal::Unknown, ItemPrincipal::A2aBound),
            Self::Unknown => ItemPrincipal::Unknown,
        }
    }
}

/// `boundPeer` as text, or `None` for anything that is not a JSON string
/// (`A4`: an unreadable bound pin folds the one address to `Unknown`, never
/// a journal error).
fn bound_peer_text<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(text) => Some(text),
        _ => None,
    })
}

#[derive(Serialize, Deserialize)]
struct ItemAddressWire {
    principal: WirePrincipal,
    item: ItemId,
    #[serde(
        default,
        rename = "boundPeer",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "bound_peer_text"
    )]
    bound_peer: Option<String>,
}

impl From<ItemAddress> for ItemAddressWire {
    fn from(address: ItemAddress) -> Self {
        let (principal, bound_peer) = WirePrincipal::split(&address.principal);
        Self {
            principal,
            item: address.item,
            bound_peer,
        }
    }
}

impl From<ItemAddressWire> for ItemAddress {
    fn from(wire: ItemAddressWire) -> Self {
        Self {
            principal: wire.principal.join(wire.bound_peer),
            item: wire.item,
        }
    }
}

/// Complete recipient-side address.
///
/// Item-id uniqueness is **global, not per principal**: [`RecipientItemAllocator`]
/// keys `claimed_ids` on the bare id and refuses one already claimed under any
/// principal, because `/team ack` and `/team remove` address an item by id
/// alone. AD-1825's per-`(provenance, PeerId)` rule is the floor this exceeds,
/// never a cap.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "ItemAddressWire", from = "ItemAddressWire")]
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

    /// Story 19.18d — construct only at the A2A ingress for a key the operator
    /// bound to the pinned peer `pin`.
    #[must_use]
    pub fn from_a2a_bound(pin: PeerId, item: ItemId) -> Self {
        Self {
            principal: ItemPrincipal::A2aBound(pin),
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
    pub message_type: super::SemanticMessageType,
    pub authorship: super::Authorship,
    pub send_authorization: super::SendAuthorization,
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
    /// The sender's retract mark (FR94-b, Story 19.16d): the **recipient
    /// host's** clock at the moment it journalled the `RecipientItemRetracted`
    /// record, or `None` while unmarked. ⛔ Never sender-supplied.
    ///
    /// An axis **orthogonal** to [`Self::state`], by totality rather than by
    /// precedent: all six `state × retracted` combinations are reachable facts
    /// in either order (the sender marks their content, the recipient takes it
    /// on or disposes of their copy, independently), so there is no invalid
    /// pair for the state payload to absorb and ⛔ folding the mark into
    /// [`RecipientItemState`] would make `AD-1827`'s single-writer state axis
    /// carry a second party's act. `Option<i64>` is the house shape for a mark
    /// (`ChatMessage.retracted_at_ms`, `TransparencyRow.retracted_at_ms`); ⛔
    /// never a bare `retracted: bool`, which would lose *when*.
    ///
    /// ⚠ The one ordering the axes do constrain is the tombstone:
    /// [`Self::retract_outcome`] refuses a retract on `Removed`, so
    /// retract-then-remove keeps the mark on the tombstone while
    /// remove-then-retract never carries one — observably different, which is
    /// honest.
    pub retracted_at_ms: Option<i64>,
    /// Story 19.18 `A5` — the id this item answers, when it arrived as a
    /// follow-up (`x-rustain-in-reply-to`), stored raw (`AD-1824`). A
    /// creation-time attribute of THIS item, ⛔ never a transition on the
    /// item it names (`AD-1827`).
    pub in_reply_to: Option<String>,
    /// Story 19.18c `A14` — `sha256` hex of the item capability presented at
    /// ingress, or `None` for a legacy item. Immutable: the first Received
    /// record wins. ⛔ Never the capability itself.
    pub capability_hash: Option<String>,
    /// Story 19.18c `A15` — whether this follow-up's presented reply
    /// capability matched its reference's nearest item at ingress.
    pub reply_capability_matched: bool,
}

/// What the retract predicate says about one item. The retract is its own
/// two-cell predicate on the orthogonal axis, ⛔ not a third act in the
/// `next_item_state` table (`AD-1827`: the item-state legality table stays
/// 2 acts × 3 states).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetractOutcome {
    /// Unmarked and still held: journal the mark.
    Mark,
    /// Already marked: no second append and no word — the first mark wins.
    /// ⚠ The same-host `RetractionError::AlreadyRetracted` refuses instead,
    /// because a local keypress on a spent control is an operator error while
    /// a repeated wire call is an ordinary retry.
    AlreadyMarked,
    /// A tombstone (`RecipientItemState::Removed`, terminal): refused aloud,
    /// exactly as both sibling acts are refused there. Marking it would write
    /// a mark on a copy the recipient no longer shows.
    RefusedRemoved,
}

impl RecipientItemView {
    /// The retract predicate — the **one** place the already-marked and the
    /// tombstone rules live. The served verb consults it before journalling;
    /// the fold applies it through [`Self::mark_retracted`] on every replay.
    #[must_use]
    pub fn retract_outcome(&self) -> RetractOutcome {
        if matches!(self.state, RecipientItemState::Removed { .. }) {
            RetractOutcome::RefusedRemoved
        } else if self.retracted_at_ms.is_some() {
            RetractOutcome::AlreadyMarked
        } else {
            RetractOutcome::Mark
        }
    }

    /// Apply a journalled retract mark. `false` when [`Self::retract_outcome`]
    /// is not [`RetractOutcome::Mark`] — first mark wins, and a tombstone is
    /// never marked. ⛔ Never touches `state` (`AD-1827`).
    pub fn mark_retracted(&mut self, at_ms: i64) -> bool {
        if self.retract_outcome() != RetractOutcome::Mark {
            return false;
        }
        self.retracted_at_ms = Some(at_ms);
        true
    }
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

    /// Mint an opaque id independent of every sender-controlled field, under
    /// the ingress principal the caller's credential resolved to.
    pub async fn allocate_a2a_ingress(
        &self,
        principal: A2aIngressPrincipal,
    ) -> Result<ItemAddress, ItemIdCollision> {
        self.claim_a2a_candidate(principal, ItemId(format!("ri_{}", nanoid::nanoid!(32))))
            .await
    }

    async fn claim_a2a_candidate(
        &self,
        principal: A2aIngressPrincipal,
        item: ItemId,
    ) -> Result<ItemAddress, ItemIdCollision> {
        let id = item.as_str().to_owned();
        let address = match principal {
            A2aIngressPrincipal::Pseudonym(peer) => ItemAddress::from_a2a_ingress(peer, item),
            A2aIngressPrincipal::Bound(pin) => ItemAddress::from_a2a_bound(pin, item),
        };
        // One lock, one check: the id is the acknowledge command's only handle,
        // so it must be unique even across principals.
        if !self.claimed_ids.lock().await.insert(id) {
            return Err(ItemIdCollision);
        }
        Ok(address)
    }
}

/// Story 19.18 `P14` — a reply turn's host-minted id (`rr_…`). A distinct type
/// from [`ItemId`], so an item lookup can never be handed a reply-turn id: the
/// item-only [`ItemAddress`] and [`ItemId::from_replay`] stay item-only.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReplyTurnId(String);

impl ReplyTurnId {
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

#[derive(Serialize, Deserialize)]
struct TurnAddressWire {
    principal: WirePrincipal,
    turn: ReplyTurnId,
    #[serde(
        default,
        rename = "boundPeer",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "bound_peer_text"
    )]
    bound_peer: Option<String>,
}

impl From<TurnAddress> for TurnAddressWire {
    fn from(address: TurnAddress) -> Self {
        let (principal, bound_peer) = WirePrincipal::split(&address.principal);
        Self {
            principal,
            turn: address.turn,
            bound_peer,
        }
    }
}

impl From<TurnAddressWire> for TurnAddress {
    fn from(wire: TurnAddressWire) -> Self {
        Self {
            principal: wire.principal.join(wire.bound_peer),
            turn: wire.turn,
        }
    }
}

/// Story 19.18 — a reply turn's complete address. The principal is the
/// **answered item's** (the thread lives under the asker's principal, `A1`),
/// so the served thread read filters it exactly like an item. Story 19.18d
/// `A4`: persisted through the same `boundPeer` shim as [`ItemAddress`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "TurnAddressWire", from = "TurnAddressWire")]
pub struct TurnAddress {
    principal: ItemPrincipal,
    turn: ReplyTurnId,
}

impl TurnAddress {
    /// A turn answering `answered`: it inherits that item's principal. There
    /// is intentionally no constructor taking a free-standing principal.
    #[must_use]
    pub fn answering(answered: &ItemAddress, turn: ReplyTurnId) -> Self {
        Self {
            principal: answered.principal.clone(),
            turn,
        }
    }

    #[must_use]
    pub fn principal(&self) -> &ItemPrincipal {
        &self.principal
    }

    #[must_use]
    pub fn turn(&self) -> &ReplyTurnId {
        &self.turn
    }
}

/// Semantic kind of a durable reply turn. The journal field is additive:
/// older records decode as `Answer`, while unfamiliar or malformed values
/// conservatively become `Unknown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReplyKind {
    #[default]
    Answer,
    Defer {
        minutes: u16,
    },
    Unknown,
}

impl ReplyKind {
    #[must_use]
    pub const fn is_answer(&self) -> bool {
        matches!(self, Self::Answer)
    }

    /// Wire token used by the additive served-thread fields.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Defer { .. } => "defer",
            Self::Unknown => "unknown",
        }
    }
}

impl Serialize for ReplyKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct(
            "ReplyKind",
            if matches!(self, Self::Defer { .. }) {
                2
            } else {
                1
            },
        )?;
        state.serialize_field("type", self.wire_name())?;
        if let Self::Defer { minutes } = self {
            state.serialize_field("minutes", minutes)?;
        }
        state.end()
    }
}

#[derive(Clone, Copy)]
enum Tag {
    Answer,
    Defer,
    Other,
}

#[derive(Clone, Copy)]
enum LooseValue {
    Tag(Tag),
    Unsigned(u64),
    Other,
}

impl<'de> Deserialize<'de> for LooseValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LooseVisitor;

        impl<'de> Visitor<'de> for LooseVisitor {
            type Value = LooseValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a reply-kind field value")
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
                Ok(LooseValue::Tag(match value {
                    "answer" => Tag::Answer,
                    "defer" => Tag::Defer,
                    _ => Tag::Other,
                }))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(LooseValue::Tag(match value {
                    "answer" => Tag::Answer,
                    "defer" => Tag::Defer,
                    _ => Tag::Other,
                }))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(LooseValue::Unsigned(value))
            }

            fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
                Ok(LooseValue::Other)
            }

            fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
                Ok(LooseValue::Other)
            }

            fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
                Ok(LooseValue::Other)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(LooseValue::Other)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(LooseValue::Other)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(LooseValue::Other)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(LooseValue::Other)
            }
        }

        deserializer.deserialize_any(LooseVisitor)
    }
}

#[derive(Deserialize)]
#[serde(field_identifier)]
enum ReplyKindField {
    #[serde(rename = "type")]
    Type,
    #[serde(rename = "minutes")]
    Minutes,
    #[serde(other)]
    Other,
}

impl<'de> Deserialize<'de> for ReplyKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ReplyKindVisitor;

        impl<'de> Visitor<'de> for ReplyKindVisitor {
            type Value = ReplyKind;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a tagged reply-kind object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut tag = None;
                let mut minutes = None;
                let mut duplicate = false;
                let mut problem = false;
                while let Some(key) = map.next_key::<ReplyKindField>()? {
                    match key {
                        ReplyKindField::Type => {
                            if tag.is_some() {
                                duplicate = true;
                            }
                            tag = Some(map.next_value::<LooseValue>()?);
                        }
                        ReplyKindField::Minutes => {
                            if minutes.is_some() {
                                duplicate = true;
                            }
                            minutes = Some(map.next_value::<LooseValue>()?);
                        }
                        ReplyKindField::Other => {
                            problem = true;
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                if duplicate || problem {
                    return Ok(ReplyKind::Unknown);
                }
                match (tag, minutes) {
                    (Some(LooseValue::Tag(Tag::Answer)), None) => Ok(ReplyKind::Answer),
                    (Some(LooseValue::Tag(Tag::Defer)), Some(LooseValue::Unsigned(value)))
                        if (1..=u64::from(u16::MAX)).contains(&value) =>
                    {
                        Ok(ReplyKind::Defer {
                            minutes: value as u16,
                        })
                    }
                    _ => Ok(ReplyKind::Unknown),
                }
            }
            fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
                Ok(ReplyKind::Unknown)
            }

            fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
                Ok(ReplyKind::Unknown)
            }

            fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
                Ok(ReplyKind::Unknown)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(ReplyKind::Unknown)
            }

            fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
                Ok(ReplyKind::Unknown)
            }

            fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
                Ok(ReplyKind::Unknown)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(ReplyKind::Unknown)
            }
        }

        deserializer.deserialize_any(ReplyKindVisitor)
    }
}

/// One reply turn, reconstructed solely from its durable
/// `RecipientItemReplied` record. ⛔ No act changes it after the append: the
/// reply turn's act set is closed (`AD-1829`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyTurnView {
    pub address: TurnAddress,
    /// The item this turn answers — always an item, never another turn.
    pub answers: ItemId,
    pub content: String,
    pub authorship: Authorship,
    pub kind: ReplyKind,
    /// The peer-response node whose card this turn resolved, when it came
    /// through a draft resolution (`Q1`). At most one turn per node.
    pub via_node: Option<AgentId>,
    /// Same scheme as [`RecipientItemView::journal_order`], and drawn from
    /// the SAME counter, so items and turns share one order (`Q3`).
    pub journal_order: u64,
}

/// The largest reply turn this host records.
pub const MAX_REPLY_BYTES: usize =
    crate::domain::services::recipient_thread_bounds::MAX_REPLY_BYTES;

/// The prefix every reply-turn id carries (Story 19.18 `AC2(d)`). One
/// spelling: the minting below and Story 19.18c's sender-side resolver, which
/// maps a reply-turn id to the item it answers, both read it.
pub const REPLY_TURN_ID_PREFIX: &str = "rr_";

/// A fresh reply-turn candidate: a distinct `rr_` prefix, so it can never
/// equal an `ri_` item id by construction, and 32 nanoid characters so it is
/// unguessable and independent of every sender-supplied value (19.16 `A5`).
#[must_use]
pub fn fresh_reply_turn_candidate() -> String {
    format!("{REPLY_TURN_ID_PREFIX}{}", nanoid::nanoid!(32))
}

/// Claim a reply-turn id. A candidate already present in durable history is
/// **refused, never re-minted** — the `ItemIdCollision` rule: a caller must
/// never receive a different id than the one paired with its durable record.
/// Pure, with the candidate injected, so a collision is forced
/// deterministically in a test.
///
/// # Errors
///
/// [`ItemIdCollision`] when `taken` reports the candidate as already present.
pub fn claim_reply_turn_id(
    candidate: String,
    taken: impl Fn(&str) -> bool,
) -> Result<ReplyTurnId, ItemIdCollision> {
    if taken(&candidate) {
        return Err(ItemIdCollision);
    }
    Ok(ReplyTurnId(candidate))
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
                .claim_a2a_candidate(
                    A2aIngressPrincipal::Pseudonym(peer),
                    ItemId::from_replay("ri_fixed")
                )
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
                .claim_a2a_candidate(
                    A2aIngressPrincipal::Bound(second_peer),
                    ItemId::from_replay("ri_fixed")
                )
                .await,
            Err(ItemIdCollision)
        );
    }

    #[test]
    fn a_reply_turn_id_collision_is_refused_without_reminting() {
        let taken = |id: &str| id == "rr_fixed";
        assert_eq!(
            claim_reply_turn_id("rr_fixed".to_owned(), taken),
            Err(ItemIdCollision)
        );
        assert_eq!(
            claim_reply_turn_id("rr_other".to_owned(), taken),
            Ok(ReplyTurnId::from_replay("rr_other"))
        );
        assert!(fresh_reply_turn_candidate().starts_with("rr_"));
    }
}
