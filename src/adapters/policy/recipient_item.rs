//! Replay-folded recipient-item projection.

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(any(test, feature = "test-instrumentation"))]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::domain::models::{
    ItemAddress, JournalEntry, JournalRecord, RecipientItemState, RecipientItemView, RoomEvent,
};

#[cfg(any(test, feature = "test-instrumentation"))]
static ITEM_TRANSITION_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(any(test, feature = "test-instrumentation"))]
pub fn recipient_item_transition_count() -> usize {
    ITEM_TRANSITION_COUNT.load(Ordering::Relaxed)
}

#[cfg(any(test, feature = "test-instrumentation"))]
pub fn reset_recipient_item_transition_count() {
    ITEM_TRANSITION_COUNT.store(0, Ordering::Relaxed);
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
            .filter(|item| item.address.principal() == principal && item.task == task)
            .max_by_key(|item| item.journal_order)
            .cloned()
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<HashMap<ItemAddress, RecipientItemView>> {
        self.items.load_full()
    }
}

fn transition_recipient_item(
    items: &mut HashMap<ItemAddress, RecipientItemView>,
    order: u64,
    event: &RoomEvent,
) {
    #[cfg(any(test, feature = "test-instrumentation"))]
    if matches!(event, RoomEvent::RecipientItemAcknowledged { .. }) {
        ITEM_TRANSITION_COUNT.fetch_add(1, Ordering::Relaxed);
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
                    content: content.clone(),
                    state: RecipientItemState::Received,
                    journal_order: order,
                });
        }
        RoomEvent::RecipientItemAcknowledged { address, alias } => {
            if let Some(item) = items.get_mut(address) {
                item.state = acknowledge_state(item.state);
                item.alias.clone_from(alias);
            }
        }
        _ => {}
    }
}

fn acknowledge_state(current: RecipientItemState) -> RecipientItemState {
    match current {
        RecipientItemState::Received | RecipientItemState::Acknowledged => {
            RecipientItemState::Acknowledged
        }
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
}
