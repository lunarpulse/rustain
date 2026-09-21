//! Story 19.16b — the per-principal enumerator behind `x-rustain-items/list`,
//! and the read-time state predicate that stops a restart re-binding a task to
//! a tombstone.
//!
//! ⚠ **Why the cross-principal keystone lives HERE and not on the wire (`A13`,
//! `A15`).** Every serving harness in this tree binds `127.0.0.1`, and
//! `a2a::server::authenticate` returns `SubmitterKey::loopback()` *before* the
//! api-key branch whenever the bind is loopback — so on the wire two different
//! `x-api-key` values are the **same** principal and a two-credential wire
//! keystone is unexecutable. The scoping is therefore proven where it is
//! decided — the projection — with two real `SubmitterKey::from_api_key`
//! values, exactly as the shipped precedent
//! `a2a_server_exec.rs::a_non_owner_gets_byte_identical_responses…` writes it.
//! The wire half (front door, tombstone in the result, no `journal_order`,
//! collapse disclosure) is proven in `tests/a2a_server.rs`.

#![cfg(feature = "a2a")]

use rustain::adapters::a2a::exec::SubmitterKey;
use rustain::adapters::policy::recipient_item::JournalRecipientItemProjection;
use rustain::domain::models::{
    ItemAddress, ItemId, ItemPrincipal, JournalEntry, JournalRecord, RecipientItemState, RoomEvent,
};

fn entry(seq: u64, event: RoomEvent) -> JournalEntry {
    JournalEntry::new(seq, JournalRecord::Room(event), seq as i64)
}

fn received(address: &ItemAddress, task: &str) -> RoomEvent {
    RoomEvent::RecipientItemReceived {
        address: address.clone(),
        task: task.to_owned(),
        alias: None,
        content: "durable content".to_owned(),
    }
}

fn address_for(key: &SubmitterKey, item: &str) -> ItemAddress {
    ItemAddress::from_a2a_ingress(key.pseudonymous_peer_id(), ItemId::from_replay(item))
}

fn principal_of(key: &SubmitterKey) -> ItemPrincipal {
    ItemPrincipal::A2aPseudonym(key.pseudonymous_peer_id())
}

/// Story 19.16b AC1(c) — the enumeration is scoped to the caller's own
/// principal, and two distinct configured credentials really are two distinct
/// principals.
///
/// **Mutant → RED:** filter on the item id instead of the principal, or read
/// the principal from a request parameter.
/// **Positive control:** A's own item IS returned — proving the filter
/// discriminates rather than returning empty for everyone.
#[test]
fn ac1_the_enumerator_returns_the_callers_own_items_and_none_of_another_principals() {
    let alpha = SubmitterKey::from_api_key("credential-alpha");
    let beta = SubmitterKey::from_api_key("credential-beta");
    assert_ne!(
        alpha.pseudonymous_peer_id(),
        beta.pseudonymous_peer_id(),
        "control: `from_api_key` digests the PRESENTED value, so two configured \
         credentials are two principals — off loopback this is the whole \
         authorization boundary"
    );

    let mine = address_for(&alpha, "ri_alpha");
    let theirs = address_for(&beta, "ri_beta");
    let entries = vec![
        entry(1, received(&mine, "task-alpha")),
        entry(2, received(&theirs, "task-beta")),
    ];
    let projection = JournalRecipientItemProjection::from_entries(&entries);

    let found = projection.find_by_principal(&principal_of(&alpha));
    assert_eq!(
        found
            .iter()
            .map(|item| item.address.clone())
            .collect::<Vec<_>>(),
        vec![mine],
        "the caller reads its own set — and exactly its own set"
    );
    assert_eq!(
        projection.find_by_principal(&principal_of(&beta)).len(),
        1,
        "control: beta's own read is not empty either, so the filter is a \
         filter and not a constant"
    );
}

/// Story 19.16b AC1(e)/`A22` — another principal's **removed** item is absent
/// from the set, byte-identically to an id that never existed.
///
/// `A3`'s `-32001` collapse held on a single-server topology; `A21` replaced it
/// with N cross-host calls, so tombstone-vs-not-found now travels per peer. A
/// handler that classifies the tombstone before filtering by principal answers
/// *"that id exists but is not yours"* — the enumeration oracle
/// `ADR-17-4a-01` R21 exists to kill.
///
/// **Mutant → RED:** swap the two steps — classify, then filter.
/// ⛔ An assertion that merely counts A's own rows passes under that swap and
/// is refused; this one compares A's read against A's read for a principal
/// that never minted anything.
#[test]
fn ac1_another_principals_tombstone_is_byte_identical_to_an_id_that_never_existed() {
    let alpha = SubmitterKey::from_api_key("credential-alpha");
    let beta = SubmitterKey::from_api_key("credential-beta");
    let never = SubmitterKey::from_api_key("credential-never-used");

    let theirs = address_for(&beta, "ri_beta_removed");
    let entries = vec![
        entry(1, received(&theirs, "task-beta")),
        entry(
            2,
            RoomEvent::RecipientItemRemoved {
                address: theirs.clone(),
            },
        ),
    ];
    let projection = JournalRecipientItemProjection::from_entries(&entries);

    // Positive control: the tombstone really is present for its OWN principal,
    // so "absent for alpha" is not vacuous.
    let owner_view = projection.find_by_principal(&principal_of(&beta));
    assert_eq!(owner_view.len(), 1);
    assert_eq!(
        owner_view[0].state,
        RecipientItemState::Removed {
            acknowledged_before: false
        }
    );

    // ⚠ Byte-identity ALONE is a false green and was measured as one: under the
    // swap mutant both principals receive the tombstone, so the two reads stay
    // equal. The assertion that bites is that the neighbour's removed item is
    // ABSENT from a read it does not own.
    let stranger = projection.find_by_principal(&principal_of(&alpha));
    assert!(
        stranger.is_empty(),
        "a tombstone classified BEFORE the principal filter answers \
         \"that id exists but is not yours\" — the cross-peer enumeration \
         oracle `ADR-17-4a-01` R21 exists to kill: {stranger:?}"
    );
    assert_eq!(
        stranger,
        projection.find_by_principal(&principal_of(&never)),
        "…and a principal whose neighbour removed an item must read \
         byte-identically to one that never minted anything"
    );
}

/// Story 19.16b AC1(f) — the enumeration order agrees across the two
/// `journal_order` assignment schemes, which is why only a **dense** ordinal
/// may reach the wire.
///
/// `from_entries` numbers by the absolute position in the journal — counting
/// the rows that are **not** recipient items; `apply` numbers by
/// `max(existing) + 1` and only ever sees item events. Both are monotonic, so
/// the ORDER matches while the NUMBERS do not.
///
/// **Mutant → RED:** serialize the raw `journal_order` — the two folds below
/// disagree on the value while agreeing on the sequence.
#[test]
fn ac1_the_enumeration_order_survives_a_cold_fold_versus_a_live_apply() {
    let key = SubmitterKey::from_api_key("credential-order");
    let first = address_for(&key, "ri_one");
    let second = address_for(&key, "ri_two");
    let events = [received(&first, "task-1"), received(&second, "task-2")];

    // A real journal interleaves other rows. The cold fold counts them; the
    // live `apply` path never sees them — which is precisely the divergence.
    let cold = JournalRecipientItemProjection::from_entries(&[
        JournalEntry::new(
            1,
            JournalRecord::AliasBound {
                node: rustain::domain::models::AgentId::root(),
                alias: "unrelated".to_owned(),
            },
            1,
        ),
        entry(2, events[0].clone()),
        entry(3, events[1].clone()),
    ]);
    let live = JournalRecipientItemProjection::default();
    for event in &events {
        live.apply(event);
    }

    let principal = principal_of(&key);
    let cold_ids: Vec<String> = cold
        .find_by_principal(&principal)
        .iter()
        .map(|item| item.address.item().as_str().to_owned())
        .collect();
    let live_ids: Vec<String> = live
        .find_by_principal(&principal)
        .iter()
        .map(|item| item.address.item().as_str().to_owned())
        .collect();
    assert_eq!(cold_ids, vec!["ri_one", "ri_two"]);
    assert_eq!(cold_ids, live_ids, "both folds agree on the ORDER");

    let cold_numbers: Vec<u64> = cold
        .find_by_principal(&principal)
        .iter()
        .map(|item| item.journal_order)
        .collect();
    let live_numbers: Vec<u64> = live
        .find_by_principal(&principal)
        .iter()
        .map(|item| item.journal_order)
        .collect();
    assert_ne!(
        cold_numbers, live_numbers,
        "…and disagree on the NUMBER, which is exactly why leaking \
         `journal_order` would make two honest hosts contradict each other"
    );
}

/// Story 19.16b AC4 — a removed item is never the item a task re-binds to, and
/// a tombstone must not shadow a live earlier execution.
///
/// This is the **defect** half at the policy layer
/// (`DF-19-16C-TOMBSTONE-BINDS-ON-RESTART`); the real restart front door is
/// exercised in `tests/a2a_server_exec.rs`.
///
/// **Mutant → RED:** drop the state predicate from `find_by_task`.
/// **Positive control:** the live item in the same fold still re-binds.
#[test]
fn ac4_a_removed_item_never_re_binds_a_task_and_never_shadows_a_live_one() {
    let key = SubmitterKey::from_api_key("credential-restart");
    let principal = principal_of(&key);

    // One task, one item, removed: nothing to re-bind to.
    let only = address_for(&key, "ri_only");
    let removed_only = JournalRecipientItemProjection::from_entries(&[
        entry(1, received(&only, "task-orphan")),
        entry(
            2,
            RoomEvent::RecipientItemRemoved {
                address: only.clone(),
            },
        ),
    ]);
    assert_eq!(
        removed_only.find_by_task(&principal, "task-orphan"),
        None,
        "a restart must not re-bind an orphaned task to a tombstone and \
         republish the removed id through the task-metadata carrier"
    );

    // A resent `messageId` mints one item per execution. Removing the NEWEST
    // must not let the tombstone shadow the live earlier one.
    let older = address_for(&key, "ri_older");
    let newer = address_for(&key, "ri_newer");
    let shadowed = JournalRecipientItemProjection::from_entries(&[
        entry(1, received(&older, "task-resent")),
        entry(2, received(&newer, "task-resent")),
        entry(
            3,
            RoomEvent::RecipientItemRemoved {
                address: newer.clone(),
            },
        ),
    ]);
    assert_eq!(
        shadowed
            .find_by_task(&principal, "task-resent")
            .map(|item| item.address),
        Some(older),
        "the newest LIVE execution binds; a removed newest must not shadow it"
    );

    // Positive control: the predicate did not simply disable re-binding.
    let live = address_for(&key, "ri_live");
    let untouched =
        JournalRecipientItemProjection::from_entries(&[entry(1, received(&live, "task-live"))]);
    assert_eq!(
        untouched
            .find_by_task(&principal, "task-live")
            .map(|item| item.address),
        Some(live),
        "a live item still re-binds across a restart"
    );

    // …and the tombstone stays readable to its OWN host, because the predicate
    // is at this read, never at the fold (`AD-1827`: 19.16c is the only writer
    // of item state).
    assert_eq!(
        removed_only
            .find_by_principal(&principal)
            .first()
            .map(|item| item.state.wire_name()),
        Some("removed"),
        "the read that must REPORT a tombstone still sees it"
    );
}
