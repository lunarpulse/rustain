//! Story 19.16d — the recipient's retract fact class, proven where it is
//! decided: the replay fold.
//!
//! ⚠ **Why the cross-principal keystone lives HERE and not on the wire
//! (`19-16b A13`/`A15`).** Every serving harness binds `127.0.0.1`, and
//! `a2a::server::authenticate` returns `SubmitterKey::loopback()` before the
//! api-key branch on a loopback bind, so on the wire two `x-api-key` values are
//! the same principal. That two configured credentials are two principals —
//! and that a retract recorded under one never marks the other's item — is
//! therefore proven on the fold with two real `SubmitterKey::from_api_key`
//! values. The wire half (front door, principal from the caller, byte-identical
//! not-found, notification refusal, idempotence, fail-closed) is in
//! `tests/a2a_server.rs`. ⛔ Neither half claims peers are separated: the
//! principal is the CREDENTIAL, and a shared key is one principal
//! (`DF-19-16D-CREDENTIAL-NOT-ATTRIBUTABLE`).

#![cfg(feature = "a2a")]

use rustain::adapters::a2a::exec::SubmitterKey;
use rustain::adapters::policy::recipient_item::JournalRecipientItemProjection;
use rustain::domain::models::{
    ItemAddress, ItemId, JournalEntry, JournalRecord, RecipientItemState, RetractOutcome, RoomEvent,
};

fn entry(seq: u64, event: RoomEvent) -> JournalEntry {
    JournalEntry::new(seq, JournalRecord::Room(event), seq as i64)
}

fn address_for(key: &SubmitterKey, item: &str) -> ItemAddress {
    ItemAddress::from_a2a_ingress(key.pseudonymous_peer_id(), ItemId::from_replay(item))
}

fn received(address: &ItemAddress) -> RoomEvent {
    RoomEvent::RecipientItemReceived {
        address: address.clone(),
        task: "task-1".to_owned(),
        alias: None,
        content: "durable content".to_owned(),
    }
}

fn retracted(address: &ItemAddress, at_ms: i64) -> RoomEvent {
    RoomEvent::RecipientItemRetracted {
        address: address.clone(),
        retracted_at_ms: at_ms,
        principal_collapsed: false,
    }
}

/// Story 19.16d AC1(d) — authorization is principal equality: a retract
/// recorded under credential A's principal never marks credential B's item,
/// even when it names B's item id.
///
/// **Mutant → RED:** key the fold's retract arm on the item id alone (e.g.
/// `find_by_id`) instead of the full address — B's item gets A's mark.
/// **Positive control:** the same record under B's own principal marks it, so
/// the check discriminates rather than refusing everything.
#[test]
fn ac1d_a_retract_under_one_credential_never_marks_another_credentials_item() {
    let a = SubmitterKey::from_api_key("credential-a");
    let b = SubmitterKey::from_api_key("credential-b");
    assert_ne!(
        a.pseudonymous_peer_id(),
        b.pseudonymous_peer_id(),
        "two configured credentials are two principals"
    );
    let theirs = address_for(&b, "ri_theirs");
    let forged = address_for(&a, "ri_theirs");

    let fold = JournalRecipientItemProjection::from_entries(&[
        entry(1, received(&theirs)),
        entry(2, retracted(&forged, 1_700_000_000_000)),
    ]);
    assert_eq!(
        fold.get(&theirs).expect("B's item").retracted_at_ms,
        None,
        "A's principal owns no item called ri_theirs, so the record marks nothing"
    );
    assert!(fold.get(&forged).is_none(), "and it mints nothing either");

    let owned = JournalRecipientItemProjection::from_entries(&[
        entry(1, received(&theirs)),
        entry(2, retracted(&theirs, 1_700_000_000_000)),
    ]);
    assert_eq!(
        owned.get(&theirs).expect("B's item").retracted_at_ms,
        Some(1_700_000_000_000),
        "positive control: B's own retract marks B's item"
    );
}

/// Story 19.16d AC3(b) — the fold arm is the highest-probability disaster: a
/// journalled retract must survive a COLD re-fold, or the mark never survives
/// a restart (`AD-1828`'s Rule names "its retract mark").
///
/// **Mutant → RED (`M10`):** delete the `RecipientItemRetracted` arm in
/// `transition_recipient_item` — `RoomEvent`'s `_ => {}` swallows it silently.
/// **Positive control:** the same cold fold still reports the item's state
/// (`Acknowledged`), proving the fold was reached rather than skipped, and the
/// mark did not touch the state axis (`AD-1827`).
#[test]
fn ac3b_a_journalled_retract_survives_a_cold_refold_and_leaves_the_state_alone() {
    let key = SubmitterKey::from_api_key("credential-a");
    let address = address_for(&key, "ri_cold");
    let entries = [
        entry(1, received(&address)),
        entry(
            2,
            RoomEvent::RecipientItemAcknowledged {
                address: address.clone(),
                alias: None,
            },
        ),
        entry(3, retracted(&address, 1_700_000_060_000)),
    ];

    let cold = JournalRecipientItemProjection::from_entries(&entries);
    let item = cold.get(&address).expect("the item folds");
    assert_eq!(
        item.retracted_at_ms,
        Some(1_700_000_060_000),
        "the retract mark survives a cold re-fold"
    );
    assert!(
        matches!(item.state, RecipientItemState::Acknowledged { .. }),
        "positive control: the fold was reached and the state axis is untouched: {:?}",
        item.state
    );

    // Live apply and cold fold agree on the mark.
    let live = JournalRecipientItemProjection::from_entries(&entries[..2]);
    live.apply(&retracted(&address, 1_700_000_060_000));
    assert_eq!(
        live.get(&address).expect("item").retracted_at_ms,
        item.retracted_at_ms
    );
}

/// Story 19.16d AC1(e) / `Q3` — the retract predicate, as the fold applies it
/// on every replay: the first mark wins, a tombstone is never marked, and the
/// two removal orderings are observably different.
#[test]
fn ac1e_the_first_mark_wins_and_a_tombstone_is_never_marked() {
    let key = SubmitterKey::from_api_key("credential-a");
    let address = address_for(&key, "ri_order");
    let removed = RoomEvent::RecipientItemRemoved {
        address: address.clone(),
    };

    let twice = JournalRecipientItemProjection::from_entries(&[
        entry(1, received(&address)),
        entry(2, retracted(&address, 100)),
        entry(3, retracted(&address, 200)),
    ]);
    assert_eq!(
        twice.get(&address).expect("item").retracted_at_ms,
        Some(100),
        "first mark wins"
    );

    let retract_then_remove = JournalRecipientItemProjection::from_entries(&[
        entry(1, received(&address)),
        entry(2, retracted(&address, 100)),
        entry(3, removed.clone()),
    ]);
    let tombstone = retract_then_remove.get(&address).expect("item");
    assert!(matches!(
        tombstone.state,
        RecipientItemState::Removed { .. }
    ));
    assert_eq!(
        tombstone.retracted_at_ms,
        Some(100),
        "a mark taken while the item was live stays on its tombstone"
    );
    assert_eq!(tombstone.retract_outcome(), RetractOutcome::RefusedRemoved);

    let remove_then_retract = JournalRecipientItemProjection::from_entries(&[
        entry(1, received(&address)),
        entry(2, removed),
        entry(3, retracted(&address, 100)),
    ]);
    assert_eq!(
        remove_then_retract
            .get(&address)
            .expect("item")
            .retracted_at_ms,
        None,
        "a tombstone is never marked, so the two orderings stay distinguishable (Q3)"
    );
}
