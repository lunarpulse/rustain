//! Replay-folded recipient-item projection.

#[cfg(any(test, feature = "test-instrumentation"))]
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::models::{
    ItemAddress, JournalEntry, JournalRecord, RecipientItemState, RecipientItemView, RoomEvent,
};

// The single-writer ratchet's counter is **per thread**, deliberately.
//
// `cargo test` runs the readers in parallel inside one binary, and every
// `from_entries` over a journal carrying an acknowledgement or a removal
// increments it — so a process-global counter makes each reader's window
// depend on the scheduler, and a ratchet that can be tricked by the scheduler
// is not evidence. Each reader folds inline on its own thread, so counting
// per thread is both deterministic and exactly as strong: a writer that
// bypasses the instrumented fold still increments nothing, which is the claim.
//
// Accepted cost, recorded (19.16c code review): the window is per thread, so a
// fold that runs on ANOTHER thread increments a counter no assertion reads —
// the pre-conversion global could observe a cross-thread fold, but it also
// made every parallel reader's assertion depend on the scheduler. Same-thread
// counting is the narrower reach, deliberately bought.
#[cfg(any(test, feature = "test-instrumentation"))]
thread_local! {
    static ITEM_TRANSITION_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(any(test, feature = "test-instrumentation"))]
pub fn recipient_item_transition_count() -> usize {
    ITEM_TRANSITION_COUNT.with(Cell::get)
}

#[cfg(any(test, feature = "test-instrumentation"))]
pub fn reset_recipient_item_transition_count() {
    ITEM_TRANSITION_COUNT.with(|count| count.set(0));
}

/// Lock-free read projection over recipient-item journal records.
#[derive(Clone)]
pub struct JournalRecipientItemProjection {
    items: Arc<arc_swap::ArcSwap<HashMap<ItemAddress, RecipientItemView>>>,
}

impl std::fmt::Debug for JournalRecipientItemProjection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JournalRecipientItemProjection")
            .field("items", &self.snapshot())
            .finish()
    }
}

impl PartialEq for JournalRecipientItemProjection {
    fn eq(&self, other: &Self) -> bool {
        self.snapshot() == other.snapshot()
    }
}

impl Eq for JournalRecipientItemProjection {}

impl Default for JournalRecipientItemProjection {
    fn default() -> Self {
        Self {
            items: Arc::new(arc_swap::ArcSwap::from_pointee(HashMap::new())),
        }
    }
}

impl JournalRecipientItemProjection {
    #[must_use]
    pub fn from_entries(entries: &[JournalEntry]) -> Self {
        let mut items = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if let JournalRecord::Room(event) = &entry.record {
                transition_recipient_item(&mut items, index as u64, event);
            }
        }
        Self {
            items: Arc::new(arc_swap::ArcSwap::from_pointee(items)),
        }
    }

    pub fn apply(&self, event: &RoomEvent) {
        self.items.rcu(|current| {
            let order = current
                .values()
                .map(|item| item.journal_order)
                .max()
                .map_or(0, |max| max + 1);
            let mut next = (**current).clone();
            transition_recipient_item(&mut next, order, event);
            Arc::new(next)
        });
    }

    pub fn replace_from(&self, entries: &[JournalEntry]) {
        let replacement = Self::from_entries(entries);
        self.items.store(replacement.items.load_full());
    }

    #[must_use]
    pub fn get(&self, address: &ItemAddress) -> Option<RecipientItemView> {
        self.items.load().get(address).cloned()
    }

    #[must_use]
    pub fn find_by_id(&self, id: &str) -> Option<RecipientItemView> {
        let snapshot = self.items.load();
        let mut matches = snapshot
            .values()
            .filter(|item| item.address.item().as_str() == id);
        let found = matches.next().cloned();
        if matches.next().is_some() {
            return None;
        }
        found
    }

    /// The item a restart's reconcile re-binds a task to, or `None`.
    ///
    /// Story 19.16b `AC4` — **the state predicate is load-bearing**
    /// (`DF-19-16C-TOMBSTONE-BINDS-ON-RESTART`). Without it a restart re-binds
    /// an orphaned task to a **tombstone** and republishes the removed id
    /// through the task-metadata carrier, presenting a disposed item to the
    /// sender as live; and because a resent `messageId` mints one item per
    /// execution, a removed newest item would *shadow* a live earlier one.
    /// ⛔ The predicate belongs here, at the **read** — `AD-1827` makes Story
    /// 19.16c the only writer of item state, and the fold must keep folding
    /// removals so the tombstone stays addressable to its own host.
    #[must_use]
    pub fn find_by_task(
        &self,
        principal: &crate::domain::models::ItemPrincipal,
        task: &str,
    ) -> Option<RecipientItemView> {
        // A resent `messageId` mints one item per execution, so several items
        // can share `(principal, task)`. The task a restart recovers is always
        // the latest execution — bind the newest item, never an arbitrary
        // `HashMap` match.
        self.items
            .load()
            .values()
            .filter(|item| {
                item.address.principal() == principal
                    && item.task == task
                    && !matches!(item.state, RecipientItemState::Removed { .. })
            })
            .max_by_key(|item| item.journal_order)
            .cloned()
    }

    /// Every item addressed to one principal, in arrival order.
    ///
    /// Story 19.16b `AC3(a)` — the server-side enumerator behind
    /// `x-rustain-items/list`. Follows [`Self::find_by_task`]'s idiom minus the
    /// task predicate and the `max_by_key`.
    ///
    /// ⛔ **The principal filter is the whole authorization boundary** and it
    /// runs before any caller classifies a tombstone (`A22`): another
    ///
    /// ⚠ Removals are **retained** here, unlike [`Self::find_by_task`]: this is
    /// the read that has to report a tombstone (`AD-1822`), not the read that
    /// re-binds a task to one.
    #[must_use]
    pub fn find_by_principal(
        &self,
        principal: &crate::domain::models::ItemPrincipal,
    ) -> Vec<RecipientItemView> {
        let snapshot = self.items.load();
        let mut owned = snapshot
            .values()
            .filter(|item| item.address.principal() == principal)
            .cloned()
            .collect::<Vec<_>>();
        // `journal_order` orders identically under both assignment schemes even
        // though the numbers differ; only the ordering is used, and only here.
        owned.sort_by_key(|item| item.journal_order);
        owned
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<HashMap<ItemAddress, RecipientItemView>> {
        self.items.load_full()
    }
}

/// One deliberate operator act on a recipient-owned durable item.
///
/// The legality of an act is a function of `(state, act)`, never of the state
/// alone — the two near-idempotent cells behave oppositely — so every caller
/// names which act it is asking about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipientItemAct {
    Acknowledge,
    Remove,
}

impl RecipientItemAct {
    /// The noun phrase this act's refusals name, so one gate serves both acts
    /// without refusing a removal in the acknowledgement's words. The
    /// acknowledge strings are unchanged byte for byte.
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::Acknowledge => "acknowledgement",
            Self::Remove => "item removal",
        }
    }
}

/// What the legality table says about one `(state, act)` pair.
///
/// Three outcomes because a caller does exactly three things: write, skip
/// silently, or refuse aloud. A bare `RecipientItemState` return cannot say
/// *refused*, and `Removed => Removed` would be byte-identical to a silent
/// no-op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemActOutcome {
    /// Legal: journal it, and the fold moves the item here.
    Applied(RecipientItemState),
    /// Already there: no write, and no word.
    Idempotent,
    /// Illegal from this state: no write, and the operator is TOLD.
    Refused(RecipientItemState),
}

/// The legality table. One exhaustive `match` over `(current, act)`, no
/// wildcard arm: a ninth state or a third act is a compile error here, which
/// is the whole reason this function exists.
///
/// | current \ act | acknowledge | remove |
/// |---|---|---|
/// | `Received` | ⇒ `Acknowledged` | ⇒ `Removed` |
/// | `Acknowledged` | idempotent (no write, no word) | ⇒ `Removed` |
/// | `Removed` | **refused** and told | **refused** and told |
///
/// The asymmetry between the two idempotent cells is deliberate: a second
/// acknowledgement is silent because the item is still there and the
/// operator's intent already holds, while a second removal is told because
/// the operator named an item they can no longer see and silence would leave
/// them unable to tell *already gone* from *never existed* — the distinction
/// AD-1822 protects on the read.
///
/// An item that was never minted is the fourth outcome and belongs to the
/// caller's lookup, not to this table.
#[must_use]
pub fn next_item_state(current: &RecipientItemState, act: RecipientItemAct) -> ItemActOutcome {
    use RecipientItemAct as Act;
    use RecipientItemState as State;
    match (current, act) {
        (State::Received { content }, Act::Acknowledge) => {
            ItemActOutcome::Applied(State::Acknowledged {
                content: content.clone(),
            })
        }
        (State::Received { .. }, Act::Remove) => ItemActOutcome::Applied(State::Removed {
            acknowledged_before: false,
        }),
        (State::Acknowledged { .. }, Act::Acknowledge) => ItemActOutcome::Idempotent,
        // The tombstone remembers which predecessor it came from — the one
        // fact `DF-19-16B-TOMBSTONE-LOSES-ACK-PREDECESSOR` needs the read to
        // keep (`A22`: a removed item renders at its last sender-visible
        // outcome). One writer of one fact: the state itself (`AD-1827`).
        (State::Acknowledged { .. }, Act::Remove) => ItemActOutcome::Applied(State::Removed {
            acknowledged_before: true,
        }),
        (State::Removed { .. }, Act::Acknowledge) => ItemActOutcome::Refused(current.clone()),
        (State::Removed { .. }, Act::Remove) => ItemActOutcome::Refused(current.clone()),
    }
}

fn transition_recipient_item(
    items: &mut HashMap<ItemAddress, RecipientItemView>,
    order: u64,
    event: &RoomEvent,
) {
    #[cfg(any(test, feature = "test-instrumentation"))]
    if matches!(
        event,
        RoomEvent::RecipientItemAcknowledged { .. } | RoomEvent::RecipientItemRemoved { .. }
    ) {
        ITEM_TRANSITION_COUNT.with(|count| count.set(count.get() + 1));
    }
    match event {
        RoomEvent::RecipientItemReceived {
            address,
            task,
            alias,
            content,
        } => {
            items
                .entry(address.clone())
                .or_insert_with(|| RecipientItemView {
                    address: address.clone(),
                    task: task.clone(),
                    alias: alias.clone(),
                    state: RecipientItemState::Received {
                        content: content.clone(),
                    },
                    journal_order: order,
                });
        }
        // The legality verdict gates the WHOLE arm, never just the state
        // assignment: `alias` is a second write with no state of its own, and
        // a journal written by a build without the removal variant folds it
        // to nothing, so that build can legally append an acknowledgement
        // after a removal. The fold refuses it here, independently of any
        // front door, on every cold re-fold.
        RoomEvent::RecipientItemAcknowledged { address, alias } => {
            if let Some(item) = items.get_mut(address)
                && let ItemActOutcome::Applied(next) =
                    next_item_state(&item.state, RecipientItemAct::Acknowledge)
            {
                item.state = next;
                item.alias.clone_from(alias);
            }
        }
        RoomEvent::RecipientItemRemoved { address } => {
            if let Some(item) = items.get_mut(address)
                && let ItemActOutcome::Applied(next) =
                    next_item_state(&item.state, RecipientItemAct::Remove)
            {
                // `next` is `Removed { acknowledged_before }` — the legality
                // table read the predecessor out of the current variant, so
                // the tombstone keeps the one fact the board's render needs
                // (`DF-19-16B-TOMBSTONE-LOSES-ACK-PREDECESSOR`, paid here).
                item.state = next;
            }
        }
        _ => {}
    }
}

/// The item one recipient-item record addresses, for the fold's own
/// post-condition. Compiled out of release builds with the assertion it feeds.
#[cfg(debug_assertions)]
fn touched_item_address(event: &RoomEvent) -> Option<&ItemAddress> {
    match event {
        RoomEvent::RecipientItemReceived { address, .. }
        | RoomEvent::RecipientItemAcknowledged { address, .. }
        | RoomEvent::RecipientItemRemoved { address } => Some(address),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ItemId, PeerId};

    #[test]
    fn every_item_state_change_routes_through_the_instrumented_transition() {
        reset_recipient_item_transition_count();
        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[17; 32]).unwrap(),
            ItemId::from_replay("ri_counter"),
        );
        let projection = JournalRecipientItemProjection::default();
        projection.apply(&RoomEvent::RecipientItemReceived {
            address: address.clone(),
            task: "sender-task".to_owned(),
            alias: None,
            content: "content".to_owned(),
        });
        reset_recipient_item_transition_count();
        projection.apply(&RoomEvent::RecipientItemAcknowledged {
            address,
            alias: None,
        });
        assert_eq!(recipient_item_transition_count(), 1);
    }

    fn entry(seq: u64, event: RoomEvent) -> JournalEntry {
        JournalEntry::new(seq, JournalRecord::Room(event), seq as i64)
    }

    /// Story 19.16c AC3(b) — **the fold refuses independently of any front
    /// door.** A journal written by a build without the removal variant folds
    /// it into `Unrecognized` and drops it, and that build's `!=` guard then
    /// legally appends an acknowledgement AFTER a removal. Every cold re-fold
    /// on this binary would replay that resurrection forever, so the refusal
    /// has to live here as well as at the handler.
    ///
    /// ⚠ This is the ONE place this story hand-builds events: the claim is
    /// precisely about a journal this rail did not write, so the sequence
    /// cannot be produced through a front door. The folded projection below is
    /// NOT presented as a production state.
    #[test]
    fn the_fold_refuses_an_acknowledgement_that_follows_a_removal() {
        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[23; 32]).unwrap(),
            ItemId::from_replay("ri_no_resurrection"),
        );
        let entries = vec![
            entry(
                1,
                RoomEvent::RecipientItemReceived {
                    address: address.clone(),
                    task: "sender-task".to_owned(),
                    alias: Some("arrival-alias".to_owned()),
                    content: "peer content".to_owned(),
                },
            ),
            entry(
                2,
                RoomEvent::RecipientItemRemoved {
                    address: address.clone(),
                },
            ),
            entry(
                3,
                RoomEvent::RecipientItemAcknowledged {
                    address: address.clone(),
                    alias: Some("clobbered-by-the-resurrection".to_owned()),
                },
            ),
        ];

        let item = JournalRecipientItemProjection::from_entries(&entries)
            .get(&address)
            .expect("the tombstone keeps its entry");
        assert!(
            matches!(item.state, RecipientItemState::Removed { .. }),
            "Removed is terminal"
        );
        // (The tombstone's content absence is now UNREPRESENTABLE — the
        // payload lives inside the state, so the old `content == None`
        // assertion has nothing left to pin.)
        assert_eq!(
            item.alias.as_deref(),
            Some("arrival-alias"),
            "the legality verdict gates the WHOLE arm: `alias` is a second, \
             unguarded write a state-only table has no reach over"
        );
    }

    /// AC3(c) positive control — the single-writer ratchet counts **both**
    /// acts. Inherited, it named one variant, so everything this story adds
    /// would have been green from birth.
    ///
    /// ⚠ Asserted in-crate against the instrumented fold, never against an
    /// operator act: the daemon rail folds BEFORE it appends and advances no
    /// stored projection, so a real `/team remove` increments this counter by
    /// zero. The forbidden-bypass rule is waived here, and only here, because
    /// the instrumented function has no other entry.
    #[test]
    fn the_transition_counter_counts_a_removal_as_well_as_an_acknowledgement() {
        let acked = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[24; 32]).unwrap(),
            ItemId::from_replay("ri_counted_ack"),
        );
        let removed = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[25; 32]).unwrap(),
            ItemId::from_replay("ri_counted_removal"),
        );
        let received = |address: &ItemAddress| RoomEvent::RecipientItemReceived {
            address: address.clone(),
            task: "sender-task".to_owned(),
            alias: None,
            content: "peer content".to_owned(),
        };
        let entries = vec![
            entry(1, received(&acked)),
            entry(2, received(&removed)),
            entry(
                3,
                RoomEvent::RecipientItemAcknowledged {
                    address: acked,
                    alias: None,
                },
            ),
            entry(4, RoomEvent::RecipientItemRemoved { address: removed }),
        ];

        reset_recipient_item_transition_count();
        let _ = JournalRecipientItemProjection::from_entries(&entries);
        assert_eq!(
            recipient_item_transition_count(),
            2,
            "one acknowledgement and one removal are two instrumented transitions"
        );
    }
}
