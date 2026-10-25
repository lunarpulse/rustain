//! Replay-folded recipient-item projection.

#[cfg(any(test, feature = "test-instrumentation"))]
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::models::{
    AgentId, ItemAddress, ItemPrincipal, ItemScope, JournalEntry, JournalRecord,
    RecipientItemState, RecipientItemView, ReplyKind, ReplyTurnView, RoomEvent, TurnAddress,
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

/// Story 19.18 `P10`/`Q3` — the ONE snapshot behind the one `ArcSwap`: items
/// and reply turns fold in the same loop, under the same order index, so a
/// thread read is a join over one projection and ⛔ there is no second store
/// (a separate store could not share the fold's watermark or its lock).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecipientItemSnapshot {
    pub items: HashMap<ItemAddress, RecipientItemView>,
    pub replies: HashMap<TurnAddress, ReplyTurnView>,
}

impl RecipientItemSnapshot {
    /// The next live-apply order: one past the highest order over BOTH maps
    /// (`Q3`). Items-only would let a live item tie with, or sort before, a
    /// reply turn that was applied first — and a restart would then reorder
    /// the thread.
    fn next_order(&self) -> u64 {
        self.items
            .values()
            .map(|item| item.journal_order)
            .chain(self.replies.values().map(|turn| turn.journal_order))
            .max()
            .map_or(0, |max| max + 1)
    }

    pub(crate) fn find_by_id(&self, id: &str) -> Option<&RecipientItemView> {
        let mut matches = self
            .items
            .values()
            .filter(|item| item.address.item().as_str() == id);
        let found = matches.next();
        if matches.next().is_some() {
            return None;
        }
        found
    }
}

/// One turn of a served thread (Story 19.18 `AC4(b)`): an item (the root or
/// a follow-up) or a reply turn, in the one fold order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadTurn {
    Item(RecipientItemView),
    Reply(ReplyTurnView),
}

impl ThreadTurn {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Item(item) => item.address.item().as_str(),
            Self::Reply(turn) => turn.address.turn().as_str(),
        }
    }

    #[must_use]
    pub fn journal_order(&self) -> u64 {
        match self {
            Self::Item(item) => item.journal_order,
            Self::Reply(turn) => turn.journal_order,
        }
    }
}

/// Borrowed member of a single folded recipient thread.
#[derive(Clone, Copy)]
pub(crate) enum ThreadTurnRef<'a> {
    Item(&'a RecipientItemView),
    Reply(&'a ReplyTurnView),
}

impl ThreadTurnRef<'_> {
    #[must_use]
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Item(item) => item.address.item().as_str(),
            Self::Reply(turn) => turn.address.turn().as_str(),
        }
    }

    fn to_owned(self) -> ThreadTurn {
        match self {
            Self::Item(item) => ThreadTurn::Item(item.clone()),
            Self::Reply(turn) => ThreadTurn::Reply(turn.clone()),
        }
    }

    fn journal_order(self) -> u64 {
        match self {
            Self::Item(item) => item.journal_order,
            Self::Reply(turn) => turn.journal_order,
        }
    }
}

/// Story 19.18c `A8` — what the served checks need of an id's nearest item:
/// its capability class. ⛔ No content, so a lookup never clones a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NearestItem {
    /// `sha256` hex of the item's capability, or `None` for a legacy item.
    pub capability_hash: Option<String>,
}

#[derive(Clone, Copy)]
struct ThreadIndexNode<'a> {
    parent: Option<&'a str>,
    member: ThreadTurnRef<'a>,
}

#[derive(Clone, Copy)]
enum ThreadRoot<'a> {
    Terminal(&'a str),
    Cycle {
        cycle: usize,
        entry: usize,
        distance: usize,
    },
}

impl<'a> ThreadRoot<'a> {
    fn parent(self) -> Self {
        match self {
            Self::Terminal(root) => Self::Terminal(root),
            Self::Cycle {
                cycle,
                entry,
                distance,
            } => Self::Cycle {
                cycle,
                entry,
                distance: distance + 1,
            },
        }
    }
}

fn resolve_thread_roots<'a>(
    index: &HashMap<&'a str, ThreadIndexNode<'a>>,
) -> HashMap<&'a str, &'a str> {
    // A cycle's result depends on the bounded walk's remaining hops. Retain
    // its entry and each predecessor's distance so every root is resolved once
    // without changing that cycle-safe behavior.
    let mut roots: HashMap<&'a str, ThreadRoot<'a>> = HashMap::with_capacity(index.len());
    let mut cycles = Vec::<Vec<&str>>::new();
    let mut path = Vec::new();
    let mut positions = HashMap::with_capacity(index.len());

    for (walk, &start) in index.keys().enumerate() {
        if roots.contains_key(start) {
            continue;
        }

        path.clear();
        let mut current = start;
        loop {
            if let Some(&root) = roots.get(current) {
                let mut successor = root;
                for &member in path.iter().rev() {
                    successor = successor.parent();
                    roots.insert(member, successor);
                }
                break;
            }

            if let Some(&(_, cycle_start)) =
                positions.get(current).filter(|(seen, _)| *seen == walk)
            {
                let cycle = path[cycle_start..].to_vec();
                let cycle_id = cycles.len();
                for (entry, &member) in cycle.iter().enumerate() {
                    roots.insert(
                        member,
                        ThreadRoot::Cycle {
                            cycle: cycle_id,
                            entry,
                            distance: 0,
                        },
                    );
                }
                cycles.push(cycle);

                let mut successor = *roots
                    .get(path[cycle_start])
                    .expect("cycle members have a root");
                for &member in path[..cycle_start].iter().rev() {
                    successor = successor.parent();
                    roots.insert(member, successor);
                }
                break;
            }

            positions.insert(current, (walk, path.len()));
            path.push(current);
            match index.get(current).and_then(|node| node.parent) {
                Some(parent) if parent != current && index.contains_key(parent) => current = parent,
                _ => {
                    for &member in &path {
                        roots.insert(member, ThreadRoot::Terminal(current));
                    }
                    break;
                }
            }
        }
    }

    let walk_limit = index.len();
    roots
        .into_iter()
        .map(|(member, root)| {
            let root = match root {
                ThreadRoot::Terminal(root) => root,
                ThreadRoot::Cycle {
                    cycle,
                    entry,
                    distance,
                } => {
                    let members = &cycles[cycle];
                    members[(entry + walk_limit.saturating_sub(distance)) % members.len()]
                }
            };
            (member, root)
        })
        .collect()
}

/// Lock-free read projection over recipient-item journal records.
#[derive(Clone)]
pub struct JournalRecipientItemProjection {
    snapshot: Arc<arc_swap::ArcSwap<RecipientItemSnapshot>>,
}

impl std::fmt::Debug for JournalRecipientItemProjection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JournalRecipientItemProjection")
            .field("snapshot", &self.snapshot())
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
            snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(
                RecipientItemSnapshot::default(),
            )),
        }
    }
}

impl JournalRecipientItemProjection {
    #[must_use]
    pub fn from_entries(entries: &[JournalEntry]) -> Self {
        let mut snapshot = RecipientItemSnapshot::default();
        for (index, entry) in entries.iter().enumerate() {
            if let JournalRecord::Room(event) = &entry.record {
                transition_recipient_item(&mut snapshot, index as u64, event);
            }
        }
        Self {
            snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(snapshot)),
        }
    }

    pub fn apply(&self, event: &RoomEvent) {
        self.snapshot.rcu(|current| {
            let order = current.next_order();
            let mut next = (**current).clone();
            transition_recipient_item(&mut next, order, event);
            Arc::new(next)
        });
    }

    pub fn replace_from(&self, entries: &[JournalEntry]) {
        let replacement = Self::from_entries(entries);
        self.snapshot.store(replacement.snapshot.load_full());
    }

    #[must_use]
    pub fn get(&self, address: &ItemAddress) -> Option<RecipientItemView> {
        self.snapshot.load().items.get(address).cloned()
    }

    #[must_use]
    pub fn find_by_id(&self, id: &str) -> Option<RecipientItemView> {
        self.snapshot.load().find_by_id(id).cloned()
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
    pub fn find_by_task(&self, principal: &ItemPrincipal, task: &str) -> Option<RecipientItemView> {
        // A resent `messageId` mints one item per execution, so several items
        // can share `(principal, task)`. The task a restart recovers is always
        // the latest execution — bind the newest item, never an arbitrary
        // `HashMap` match.
        self.snapshot
            .load()
            .items
            .values()
            .filter(|item| {
                item.address.principal() == principal
                    && item.task == task
                    && !matches!(item.state, RecipientItemState::Removed { .. })
            })
            .max_by_key(|item| item.journal_order)
            .cloned()
    }

    /// Every item readable in one caller's scope, in arrival order.
    ///
    /// Story 19.16b `AC3(a)` — the server-side enumerator behind
    /// `x-rustain-items/list`. Story 19.18d `A11`/`P8`: one membership filter
    /// over the deduplicated scope, then ONE global `journal_order` sort — never
    /// a per-principal concatenation.
    ///
    /// ⛔ **The scope filter is the whole authorization boundary** and it runs
    /// before any caller classifies a tombstone (`A22`): another principal's
    /// removed item is absent, byte-identical to an id that never existed.
    ///
    /// ⚠ Removals are **retained** here, unlike [`Self::find_by_task`]: this is
    /// the read that has to report a tombstone (`AD-1822`), not the read that
    /// re-binds a task to one.
    #[must_use]
    pub fn find_in_scope(&self, scope: &ItemScope) -> Vec<RecipientItemView> {
        let snapshot = self.snapshot.load();
        let mut owned = snapshot
            .items
            .values()
            .filter(|item| scope.contains(item.address.principal()))
            .cloned()
            .collect::<Vec<_>>();
        // `journal_order` orders identically under both assignment schemes even
        // though the numbers differ; only the ordering is used, and only here.
        owned.sort_by_key(|item| item.journal_order);
        owned
    }

    /// Story 19.18d `F6` — the item with id `id` readable in `scope`, or
    /// `None`. Item ids are globally unique, so at most one matches; the
    /// retract then works on this item's **stored** address.
    #[must_use]
    pub fn item_in_scope(&self, scope: &ItemScope, id: &str) -> Option<RecipientItemView> {
        self.snapshot
            .load()
            .items
            .values()
            .find(|item| {
                scope.contains(item.address.principal()) && item.address.item().as_str() == id
            })
            .cloned()
    }

    /// Story 19.18 `AC4(f)` — how many reply turns directly answer `item`
    /// (the board's `· <n> replies`). Same principal by construction: a
    /// turn's address inherits its answered item's principal.
    #[must_use]
    pub fn reply_count(&self, item: &ItemAddress) -> usize {
        self.snapshot
            .load()
            .replies
            .values()
            .filter(|turn| {
                turn.address.principal() == item.principal() && &turn.answers == item.item()
            })
            .count()
    }

    /// Story 19.18 `Q1` — the reply turn that resolved `node`'s card, if one
    /// is durable. The journal, not the draft controller, holds "one turn
    /// per node".
    #[must_use]
    pub fn reply_for_node(&self, node: &AgentId) -> Option<ReplyTurnView> {
        self.snapshot
            .load()
            .replies
            .values()
            .find(|turn| turn.via_node.as_ref() == Some(node))
            .cloned()
    }

    /// Whether any reply turn — under ANY principal — already holds `id`.
    /// The uniqueness half of reply-turn minting (`AC2(d)`).
    #[must_use]
    pub fn holds_reply_turn_id(&self, id: &str) -> bool {
        self.snapshot
            .load()
            .replies
            .keys()
            .any(|address| address.turn().as_str() == id)
    }

    /// Story 19.18c `A8` — the capability class of `id`'s **nearest item**
    /// inside `scope`: the item itself for an item id, the item a reply turn
    /// answers for a turn id. `None` when `id` names nothing in this scope —
    /// byte-identical, for a caller, to one that never existed (R21).
    #[must_use]
    pub fn nearest_item(&self, scope: &ItemScope, id: &str) -> Option<NearestItem> {
        let snapshot = self.snapshot.load();
        let item_of = |item_id: &str| {
            snapshot.items.values().find(|item| {
                scope.contains(item.address.principal()) && item.address.item().as_str() == item_id
            })
        };
        let item = item_of(id).or_else(|| {
            snapshot
                .replies
                .values()
                .find(|turn| {
                    scope.contains(turn.address.principal()) && turn.address.turn().as_str() == id
                })
                .and_then(|turn| item_of(turn.answers.as_str()))
        })?;
        Some(NearestItem {
            capability_hash: item.capability_hash.clone(),
        })
    }

    /// Story 19.18c `A22`/`P16` — the capability class of every item sharing
    /// `(principal, task)`, **Removed included**, in fold order (`None` is
    /// the legacy class). Unlike [`Self::find_by_task`] a tombstone counts:
    /// the class of a task id is fixed by its first item, whatever became of
    /// it.
    #[must_use]
    pub fn task_capability_classes(
        &self,
        principal: &ItemPrincipal,
        task: &str,
    ) -> Vec<Option<String>> {
        let snapshot = self.snapshot.load();
        let mut items = snapshot
            .items
            .values()
            .filter(|item| item.address.principal() == principal && item.task == task)
            .collect::<Vec<_>>();
        items.sort_by_key(|item| item.journal_order);
        items
            .into_iter()
            .map(|item| item.capability_hash.clone())
            .collect()
    }

    /// Story 19.18 `AC4(b)` — the thread containing `id` within one caller's
    /// scope, in the one fold order, or `None` when `id` names nothing there.
    ///
    /// ⛔ **Scope first** (`A6`): only this scope's items and turns are
    /// indexed, so another principal's id is absent — byte-identical to one
    /// that never existed — and an `in_reply_to` outside the scope ends the
    /// walk instead of crossing into it. Story 19.18d `A11`/`P7`: inside the
    /// scope an edge may cross principal variants only where the capability
    /// class below already permits it.
    ///
    /// `id` resolves to its **root** by walking `in_reply_to` (items) and
    /// `answers` (turns) upward. The walk is bounded by the index size and
    /// cycle-safe; a missing parent ends it. Roots are memoized over the
    /// borrowed index, then only returned turns are cloned from the snapshot.
    ///
    /// Story 19.18c `A15` — an item's `in_reply_to` edge counts only within
    /// one capability class: legacy under legacy, or capability-bearing
    /// under capability-bearing **and** matched at ingress. Every other edge
    /// is cut and the child is its own root, so a legacy root never serves a
    /// capability-bearing descendant to a caller without its capability, and
    /// an unmatched link never joins a capability-bearing thread. Reply-turn
    /// edges (minted by this host's operator) always count.
    #[must_use]
    pub fn thread(&self, scope: &ItemScope, id: &str) -> Option<(String, Vec<ThreadTurn>)> {
        let snapshot = self.snapshot.load_full();
        let (root, turns) = Self::thread_in_snapshot(&snapshot, scope, id)?;
        Some((
            root,
            turns.into_iter().map(ThreadTurnRef::to_owned).collect(),
        ))
    }

    /// Borrowed thread view for bounded local readers. The caller retains the
    /// snapshot Arc for the returned references' lifetime; no turn bodies are
    /// cloned while applying page bounds.
    pub(crate) fn thread_in_snapshot<'a>(
        snapshot: &'a RecipientItemSnapshot,
        scope: &ItemScope,
        id: &str,
    ) -> Option<(String, Vec<ThreadTurnRef<'a>>)> {
        let items: HashMap<&str, &RecipientItemView> = snapshot
            .items
            .values()
            .filter(|item| scope.contains(item.address.principal()))
            .map(|item| (item.address.item().as_str(), item))
            .collect();
        let answered: HashMap<&str, &str> = snapshot
            .replies
            .values()
            .filter(|turn| scope.contains(turn.address.principal()))
            .map(|turn| (turn.address.turn().as_str(), turn.answers.as_str()))
            .collect();
        let nearest_class = |reference: &str| {
            items
                .get(reference)
                .or_else(|| answered.get(reference).and_then(|item| items.get(item)))
                .map(|item| item.capability_hash.is_some())
        };
        let mut index = HashMap::new();
        for (&item_id, &item) in &items {
            let parent = item.in_reply_to.as_deref().filter(|reference| {
                match (item.capability_hash.is_some(), nearest_class(reference)) {
                    (_, None) => true,
                    (false, Some(false)) => true,
                    (true, Some(true)) => item.reply_capability_matched,
                    (false, Some(true)) | (true, Some(false)) => false,
                }
            });
            index.insert(
                item_id,
                ThreadIndexNode {
                    parent,
                    member: ThreadTurnRef::Item(item),
                },
            );
        }
        for turn in snapshot
            .replies
            .values()
            .filter(|turn| scope.contains(turn.address.principal()))
        {
            index.insert(
                turn.address.turn().as_str(),
                ThreadIndexNode {
                    parent: Some(turn.answers.as_str()),
                    member: ThreadTurnRef::Reply(turn),
                },
            );
        }
        if !index.contains_key(id) {
            return None;
        }
        let roots = resolve_thread_roots(&index);
        let root = roots.get(id).copied()?;
        let mut members: Vec<ThreadTurnRef<'_>> = index
            .iter()
            .filter(|(member, _)| roots.get(*member) == Some(&root))
            .map(|(_, node)| node.member)
            .collect();
        members.sort_by_key(|member| member.journal_order());
        Some((root.to_owned(), members))
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<RecipientItemSnapshot> {
        self.snapshot.load_full()
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

fn transition_recipient_item(snapshot: &mut RecipientItemSnapshot, order: u64, event: &RoomEvent) {
    let items = &mut snapshot.items;
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
            message_type,
            stories,
            selected_for,
            authorship,
            send_authorization,
            content,
            in_reply_to,
            capability_hash,
            reply_capability_matched,
        } => {
            items
                .entry(address.clone())
                .or_insert_with(|| RecipientItemView {
                    address: address.clone(),
                    task: task.clone(),
                    alias: alias.clone(),
                    message_type: *message_type,
                    stories: stories.clone(),
                    selected_for: selected_for.clone(),
                    authorship: *authorship,
                    send_authorization: *send_authorization,
                    state: RecipientItemState::Received {
                        content: content.clone(),
                    },
                    journal_order: order,
                    retracted_at_ms: None,
                    in_reply_to: in_reply_to.clone(),
                    // Story 19.18c `A14` — the first Received wins, so the
                    // hash is immutable per item.
                    capability_hash: capability_hash.clone(),
                    reply_capability_matched: *reply_capability_matched,
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
        // Story 19.16d `AC3(b)` — ⛔ NOT compile-forced: `RoomEvent` is
        // `#[non_exhaustive]` with `#[serde(other)] Unrecognized`, so without
        // this arm a journalled retract folds to nothing on every cold
        // re-fold and the mark never survives a restart (`AD-1828`'s Rule
        // names "its retract mark" among the facts that must).
        //
        // The mark is an orthogonal axis: ⛔ `item.state` is never touched
        // (`AD-1827`'s single writer). `mark_retracted` is the one expression
        // of the rule — first mark wins, a tombstone is never marked — so the
        // served verb's decision and every replay agree by construction.
        //
        // ⛔ Deliberately absent from the `ITEM_TRANSITION_COUNT` matcher
        // above: that counter's contract is *item-state transitions*, and a
        // retract writes no state.
        RoomEvent::RecipientItemRetracted {
            address,
            retracted_at_ms,
            ..
        } => {
            if let Some(item) = items.get_mut(address) {
                item.mark_retracted(*retracted_at_ms);
            }
        }
        // Story 19.18 `A4`/`P10` — a reply turn folds into the SAME snapshot,
        // under the same order counter, and ⛔ never touches the answered
        // item (`AD-1827`). Not compile-forced (`RoomEvent` is
        // `#[non_exhaustive]`), so without this arm every turn would vanish
        // on a cold re-fold and a restart would lose it (`M22`). The first
        // record for an address wins, like `RecipientItemReceived`.
        RoomEvent::RecipientItemReplied {
            address,
            answers,
            content,
            authorship,
            kind,
            status,
            retro,
            via_node,
            ..
        } => {
            let parsed_status = status
                .as_ref()
                .and_then(crate::domain::models::StatusStatement::from_wire);
            let parsed_retro = retro
                .as_ref()
                .and_then(crate::domain::models::RetroContribution::from_wire);
            let kind = if (status.is_some() && parsed_status.is_none())
                || (retro.is_some() && parsed_retro.is_none())
                || (*kind == ReplyKind::Retro && parsed_retro.is_none())
            {
                ReplyKind::Unknown
            } else {
                *kind
            };
            snapshot
                .replies
                .entry(address.clone())
                .or_insert_with(|| ReplyTurnView {
                    address: address.clone(),
                    answers: answers.clone(),
                    content: content.clone(),
                    authorship: *authorship,
                    kind,
                    status: parsed_status,
                    retro: parsed_retro,
                    via_node: via_node.clone(),
                    journal_order: order,
                });
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
        | RoomEvent::RecipientItemRemoved { address }
        | RoomEvent::RecipientItemRetracted { address, .. } => Some(address),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ItemId, PeerId};

    #[test]
    fn story19_25_journal_retro_round_trip_and_tolerance() {
        let legacy = r#"{"event":"recipient_item_replied","address":{"principal":{"kind":"unknown"},"turn":"rr_old"},"answers":"ri_old","content":"answer","authorship":"human-written"}"#;
        let old: RoomEvent = serde_json::from_str(legacy).unwrap();
        assert_eq!(serde_json::to_string(&old).unwrap(), legacy);
        let mut raw: serde_json::Value = serde_json::from_str(legacy).unwrap();
        raw["kind"] = serde_json::json!({"type":"retro"});
        raw["retro"] = serde_json::json!({});
        let event: RoomEvent = serde_json::from_value(raw.clone()).unwrap();
        let projection = JournalRecipientItemProjection::from_entries(&[entry(1, event)]);
        let snapshot = projection.snapshot();
        let turn = snapshot.replies.values().next().unwrap();
        assert_eq!(turn.kind.wire_name(), "retro");
        assert_eq!(
            serde_json::to_value(turn).unwrap()["retro"],
            serde_json::json!({})
        );
        for bad in [
            serde_json::Value::Null,
            serde_json::json!({"replyTimes":[2,1]}),
            serde_json::json!({"received":{}}),
        ] {
            raw["retro"] = bad;
            let event: RoomEvent =
                serde_json::from_value(raw.clone()).expect("not a corrupt journal line");
            let projection = JournalRecipientItemProjection::from_entries(&[entry(1, event)]);
            assert_eq!(
                projection.snapshot().replies.values().next().unwrap().kind,
                ReplyKind::Unknown
            );
        }
    }

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
            message_type: crate::domain::models::SemanticMessageType::Unknown,
            stories: None,
            selected_for: None,
            content: "content".to_owned(),
            in_reply_to: None,
            capability_hash: None,
            reply_capability_matched: false,
            authorship: crate::domain::models::Authorship::Unknown,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
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

    #[test]
    fn story19_24_journal_status_is_tolerant_and_legacy_bytes_stay_identical() {
        let legacy = r#"{"event":"recipient_item_replied","address":{"principal":{"kind":"unknown"},"turn":"rr_old"},"answers":"ri_old","content":"answer","authorship":"human-written"}"#;
        let event: RoomEvent = serde_json::from_str(legacy).expect("literal legacy reply");
        assert_eq!(serde_json::to_string(&event).unwrap(), legacy);
        let projection = JournalRecipientItemProjection::from_entries(&[entry(1, event)]);
        let snapshot = projection.snapshot();
        let old = snapshot.replies.values().next().unwrap();
        assert_eq!(old.kind, ReplyKind::Answer);
        assert_eq!(old.status, None);

        let mut raw: serde_json::Value = serde_json::from_str(legacy).unwrap();
        raw["kind"] = serde_json::json!({"type": "status"});
        let payloads = [
            serde_json::Value::Null,
            serde_json::json!([]),
            serde_json::json!({"stories": "no"}),
            serde_json::json!({"level": "status", "stories": [{"key":"S1"}]}),
            serde_json::json!({"level": "minimal", "stories": []}),
            serde_json::json!({"level":"minimal","stories":(0..65).map(|i| serde_json::json!({"key":format!("S{i}")})).collect::<Vec<_>>()}),
        ];
        for status in payloads {
            raw["status"] = status;
            let event: RoomEvent = serde_json::from_value(raw.clone())
                .expect("malformed status is not a corrupt journal record");
            let projection = JournalRecipientItemProjection::from_entries(&[entry(1, event)]);
            let snapshot = projection.snapshot();
            let reply = snapshot.replies.values().next().unwrap();
            assert_eq!(reply.kind, ReplyKind::Unknown, "{}", raw["status"]);
            assert_eq!(reply.status, None);
        }
        raw["status"] = serde_json::json!({"level":"minimal","stories":[{"key":"S1"}]});
        let event = serde_json::from_value(raw.clone()).unwrap();
        let projection = JournalRecipientItemProjection::from_entries(&[entry(1, event)]);
        let snapshot = projection.snapshot();
        let reply = snapshot.replies.values().next().unwrap();
        assert_eq!(reply.kind, ReplyKind::Status);
        assert_eq!(reply.status.as_ref().unwrap().to_wire(), raw["status"]);
        raw.as_object_mut().unwrap().remove("status");
        let event = serde_json::from_value(raw).unwrap();
        let projection = JournalRecipientItemProjection::from_entries(&[entry(1, event)]);
        let snapshot = projection.snapshot();
        let reply = snapshot.replies.values().next().unwrap();
        assert_eq!(
            reply.kind,
            ReplyKind::Status,
            "an absent field preserves the legacy kind"
        );
        assert_eq!(reply.status, None);
    }

    #[test]
    fn story19_22_journal_verdict_tolerance_preserves_the_reply() {
        let legacy = r#"{"event":"recipient_item_replied","address":{"principal":{"kind":"unknown"},"turn":"rr_old"},"answers":"ri_old","content":"answer","authorship":"human-written"}"#;
        let legacy_event: RoomEvent = serde_json::from_str(legacy).unwrap();
        assert_eq!(serde_json::to_string(&legacy_event).unwrap(), legacy);
        for (token, kind) in [
            ("accept", ReplyKind::Accept),
            ("discuss", ReplyKind::Discuss),
        ] {
            for extra in [false, true] {
                let mut raw: serde_json::Value = serde_json::from_str(legacy).unwrap();
                raw["kind"] = serde_json::json!({"type":token});
                if extra {
                    raw["kind"]["x"] = serde_json::json!(1);
                }
                let event: RoomEvent = serde_json::from_value(raw)
                    .expect("extra verdict fields are not a corrupt journal record");
                let encoded = serde_json::to_string(&event).unwrap();
                let decoded: RoomEvent = serde_json::from_str(&encoded).unwrap();
                let projection = JournalRecipientItemProjection::from_entries(&[entry(1, decoded)]);
                let snapshot = projection.snapshot();
                let reply = snapshot.replies.values().next().unwrap();
                assert_eq!(reply.kind, if extra { ReplyKind::Unknown } else { kind });
                assert_eq!(reply.content, "answer");
                assert!(reply.status.is_none());
            }
        }
    }

    fn story_event(
        address: &ItemAddress,
        stories: Option<crate::domain::models::ReceivedStories>,
    ) -> RoomEvent {
        RoomEvent::RecipientItemReceived {
            address: address.clone(),
            task: "story-task".to_owned(),
            alias: Some("sender".to_owned()),
            message_type: crate::domain::models::SemanticMessageType::StoryAssignment,
            stories,
            selected_for: None,
            authorship: crate::domain::models::Authorship::Unknown,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            content: "story content".to_owned(),
            in_reply_to: None,
            capability_hash: None,
            reply_capability_matched: false,
        }
    }

    #[test]
    fn story19_23_selection_is_first_record_immutable_and_future_tag_tolerant() {
        use crate::domain::models::{ReceivedSelection, StoryKey};
        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[31; 32]).unwrap(),
            ItemId::from_replay("ri-selected"),
        );
        let legacy = serde_json::to_string(&story_event(&address, None)).unwrap();
        let event: RoomEvent = serde_json::from_str(&legacy).unwrap();
        assert_eq!(serde_json::to_string(&event).unwrap(), legacy);
        for (raw, expected) in [
            (
                serde_json::json!({"state":"Listed","stories":["S1"]}),
                ReceivedSelection::Listed {
                    stories: vec![StoryKey::parse("S1").unwrap()],
                },
            ),
            (
                serde_json::json!({"state":"Unreadable"}),
                ReceivedSelection::Unreadable,
            ),
            (
                serde_json::json!({"state":"Future","private":"not-retained"}),
                ReceivedSelection::Unrecognized,
            ),
        ] {
            let mut value: serde_json::Value = serde_json::from_str(&legacy).unwrap();
            value["selected_for"] = raw;
            let first: RoomEvent =
                serde_json::from_value(value).expect("future selection is not a corrupt record");
            let projection = JournalRecipientItemProjection::from_entries(&[entry(1, first)]);
            let mut duplicate = story_event(&address, None);
            if let RoomEvent::RecipientItemReceived { selected_for, .. } = &mut duplicate {
                *selected_for = Some(ReceivedSelection::Unreadable);
            }
            projection.apply(&duplicate);
            projection.apply(&RoomEvent::RecipientItemAcknowledged {
                address: address.clone(),
                alias: None,
            });
            assert_eq!(
                projection.get(&address).unwrap().selected_for,
                Some(expected.clone())
            );
            projection.apply(&RoomEvent::RecipientItemRemoved {
                address: address.clone(),
            });
            assert_eq!(
                projection.get(&address).unwrap().selected_for,
                Some(expected)
            );
        }
    }

    #[test]
    fn received_stories_are_fixed_through_duplicates_acknowledgement_and_removal() {
        use crate::domain::models::{ReceivedStories, ReceivedStory, StoryHolder, StoryKey};

        let peer = PeerId::from_public_key(&[26; 32]).unwrap();
        let address =
            ItemAddress::from_a2a_ingress(peer.clone(), ItemId::from_replay("ri_story_history"));
        let list = ReceivedStories::Listed {
            stories: vec![
                ReceivedStory {
                    key: StoryKey::parse("S1").unwrap(),
                    holder: StoryHolder::You,
                },
                ReceivedStory {
                    key: StoryKey::parse("S2").unwrap(),
                    holder: StoryHolder::Member {
                        alias: "other-member".to_owned(),
                        peer: Some(peer),
                    },
                },
            ],
        };
        for first in [None, Some(list)] {
            let events = [
                story_event(&address, first.clone()),
                story_event(&address, Some(ReceivedStories::Unreadable)),
                RoomEvent::RecipientItemAcknowledged {
                    address: address.clone(),
                    alias: Some("renamed-sender".to_owned()),
                },
                story_event(&address, None),
                RoomEvent::RecipientItemRemoved {
                    address: address.clone(),
                },
            ];
            let projection = JournalRecipientItemProjection::default();
            for (index, event) in events.iter().enumerate() {
                projection.apply(event);
                let item = projection.get(&address).unwrap();
                assert_eq!(item.stories, first, "creation wins at event {index}");
                if index == 2 || index == 3 {
                    assert!(matches!(
                        item.state,
                        RecipientItemState::Acknowledged { .. }
                    ));
                    assert_eq!(item.alias.as_deref(), Some("renamed-sender"));
                }
            }
            let entries: Vec<_> = events
                .into_iter()
                .enumerate()
                .map(|(index, event)| entry(index as u64 + 1, event))
                .collect();
            let replayed = JournalRecipientItemProjection::from_entries(&entries)
                .get(&address)
                .unwrap();
            assert_eq!(replayed.stories, first);
            assert!(matches!(replayed.state, RecipientItemState::Removed { .. }));
        }
    }

    #[test]
    fn future_story_list_and_holder_tags_remain_readable_journal_records() {
        use crate::domain::models::{ReceivedStories, ReceivedStory, StoryHolder, StoryKey};
        use serde_json::json;

        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[27; 32]).unwrap(),
            ItemId::from_replay("ri_future_stories"),
        );
        for (raw, expected) in [
            (
                json!({"state": "FutureList", "future": {"private": "extension"}}),
                ReceivedStories::Unrecognized,
            ),
            (
                json!({
                    "state": "Listed",
                    "stories": [{"key": "S1", "holder": {"kind": "FutureHolder", "extra": 1}}]
                }),
                ReceivedStories::Listed {
                    stories: vec![ReceivedStory {
                        key: StoryKey::parse("S1").unwrap(),
                        holder: StoryHolder::Unrecognized,
                    }],
                },
            ),
        ] {
            let mut value = serde_json::to_value(entry(
                1,
                story_event(&address, Some(ReceivedStories::Unreadable)),
            ))
            .unwrap();
            *value.pointer_mut("/record/payload/stories").unwrap() = raw;
            let decoded: JournalEntry =
                serde_json::from_value(value).expect("future story tags keep the journal readable");
            assert!(matches!(
                &decoded.record,
                JournalRecord::Room(RoomEvent::RecipientItemReceived { .. })
            ));
            let entries = [
                decoded,
                entry(
                    2,
                    RoomEvent::RecipientItemAcknowledged {
                        address: address.clone(),
                        alias: None,
                    },
                ),
            ];
            let item = JournalRecipientItemProjection::from_entries(&entries)
                .get(&address)
                .expect("future tags do not discard the item");
            assert_eq!(item.stories, Some(expected));
            assert!(matches!(
                item.state,
                RecipientItemState::Acknowledged { .. }
            ));
        }
    }

    #[test]
    fn legacy_received_event_omits_story_field_and_folds_without_a_list() {
        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[28; 32]).unwrap(),
            ItemId::from_replay("ri_legacy_stories"),
        );
        let value = serde_json::to_value(story_event(&address, None)).unwrap();
        assert!(value.get("stories").is_none());
        let event: RoomEvent = serde_json::from_value(value).unwrap();
        let projection = JournalRecipientItemProjection::default();
        projection.apply(&event);
        assert_eq!(projection.get(&address).unwrap().stories, None);
    }

    #[test]
    fn journal_reply_kind_future_and_malformed_events_do_not_poison_fold() {
        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[24; 32]).unwrap(),
            ItemId::from_replay("ri_kind_replay"),
        );
        let received = RoomEvent::RecipientItemReceived {
            address: address.clone(),
            task: "sender-task".to_owned(),
            alias: None,
            message_type: crate::domain::models::SemanticMessageType::Consultation,
            stories: None,
            selected_for: None,
            content: "question".to_owned(),
            in_reply_to: None,
            capability_hash: None,
            reply_capability_matched: false,
            authorship: crate::domain::models::Authorship::Unknown,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
        };
        let reply = |turn: &str, kind: ReplyKind| RoomEvent::RecipientItemReplied {
            retro: None,
            address: TurnAddress::answering(
                &address,
                crate::domain::models::ReplyTurnId::from_replay(turn),
            ),
            answers: address.item().clone(),
            content: "Busy — defer 30m".to_owned(),
            kind,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
            via_node: None,
            status: None,
        };
        let mut entries = vec![entry(1, received)];
        for (seq, turn, malformed) in [(2, "rr_future", false), (3, "rr_malformed", true)] {
            let mut value =
                serde_json::to_value(entry(seq, reply(turn, ReplyKind::Defer { minutes: 30 })))
                    .unwrap();
            let mut stack = vec![&mut value];
            while let Some(node) = stack.pop() {
                match node {
                    serde_json::Value::Object(fields) => {
                        if fields.get("type").and_then(serde_json::Value::as_str) == Some("defer") {
                            fields.insert(
                                "type".to_owned(),
                                serde_json::Value::from(if malformed { "defer" } else { "future" }),
                            );
                            if malformed {
                                fields.insert(
                                    "minutes".to_owned(),
                                    serde_json::Value::from("thirty"),
                                );
                            }
                        }
                        stack.extend(fields.values_mut());
                    }
                    serde_json::Value::Array(values) => stack.extend(values.iter_mut()),
                    _ => {}
                }
            }
            let bytes = serde_json::to_vec(&value).unwrap();
            entries.push(
                serde_json::from_slice(&bytes)
                    .expect("bad reply metadata remains a valid journal entry"),
            );
        }
        for (seq, turn, minutes) in [(4, "rr_min", 1), (5, "rr_max", u16::MAX)] {
            let value = serde_json::to_value(entry(seq, reply(turn, ReplyKind::Defer { minutes })))
                .unwrap();
            let bytes = serde_json::to_vec(&value).unwrap();
            entries.push(
                serde_json::from_slice(&bytes).expect("valid interval remains a journal entry"),
            );
        }
        let legacy = serde_json::to_value(entry(6, reply("rr_legacy", ReplyKind::Answer))).unwrap();
        let mut legacy = legacy;
        if let Some(reply) = legacy.pointer_mut("/record/payload") {
            reply.as_object_mut().unwrap().remove("kind");
        }
        let legacy_bytes = serde_json::to_vec(&legacy).unwrap();
        entries.push(
            serde_json::from_slice(&legacy_bytes).expect("legacy answer line remains loadable"),
        );
        entries.push(entry(
            7,
            RoomEvent::RecipientItemAcknowledged {
                address: address.clone(),
                alias: None,
            },
        ));
        let projection = JournalRecipientItemProjection::from_entries(&entries);
        let item = projection
            .get(&address)
            .expect("later item state still folds");
        assert!(matches!(
            item.state,
            RecipientItemState::Acknowledged { .. }
        ));
        let (_, turns) = projection
            .thread(
                &ItemScope::exact(address.principal().clone()),
                address.item().as_str(),
            )
            .expect("whole thread remains readable after malformed metadata");
        let reply_kinds: Vec<_> = turns
            .iter()
            .filter_map(|turn| match turn {
                ThreadTurn::Reply(reply) => Some(reply.kind),
                ThreadTurn::Item(_) => None,
            })
            .collect();
        assert_eq!(
            reply_kinds,
            [
                ReplyKind::Unknown,
                ReplyKind::Unknown,
                ReplyKind::Defer { minutes: 1 },
                ReplyKind::Defer { minutes: u16::MAX },
                ReplyKind::Answer,
            ]
        );
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
                    message_type: crate::domain::models::SemanticMessageType::Unknown,
                    stories: None,
                    selected_for: None,
                    authorship: crate::domain::models::Authorship::Unknown,
                    send_authorization: crate::domain::models::SendAuthorization::Unknown,
                    alias: Some("arrival-alias".to_owned()),
                    content: "peer content".to_owned(),
                    in_reply_to: None,
                    capability_hash: None,
                    reply_capability_matched: false,
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
            message_type: crate::domain::models::SemanticMessageType::Unknown,
            stories: None,
            selected_for: None,
            alias: None,
            content: "peer content".to_owned(),
            in_reply_to: None,
            capability_hash: None,
            reply_capability_matched: false,
            authorship: crate::domain::models::Authorship::Unknown,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
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

    fn thread_fixture() -> (Vec<RoomEvent>, crate::domain::models::ItemPrincipal) {
        use crate::domain::models::{Authorship, ReplyTurnId, TurnAddress};

        let asker = PeerId::from_public_key(&[31; 32]).unwrap();
        let other = PeerId::from_public_key(&[32; 32]).unwrap();
        let item =
            |peer: &PeerId, id: &str, reply_to: Option<&str>| RoomEvent::RecipientItemReceived {
                address: ItemAddress::from_a2a_ingress(peer.clone(), ItemId::from_replay(id)),
                message_type: crate::domain::models::SemanticMessageType::Unknown,
                stories: None,
                selected_for: None,
                task: format!("task-{id}"),
                alias: None,
                content: format!("content {id}"),
                in_reply_to: reply_to.map(str::to_owned),
                capability_hash: None,
                reply_capability_matched: false,
                authorship: crate::domain::models::Authorship::Unknown,
                send_authorization: crate::domain::models::SendAuthorization::Unknown,
            };
        let reply = |answered: &str, turn: &str| RoomEvent::RecipientItemReplied {
            retro: None,
            address: TurnAddress::answering(
                &ItemAddress::from_a2a_ingress(asker.clone(), ItemId::from_replay(answered)),
                ReplyTurnId::from_replay(turn),
            ),
            answers: ItemId::from_replay(answered),
            content: format!("reply {turn}"),
            authorship: Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
            kind: ReplyKind::Answer,
            via_node: None,
            status: None,
        };
        let events = vec![
            item(&asker, "ri_q", None),
            reply("ri_q", "rr_1"),
            reply("ri_q", "rr_2"),
            item(&asker, "ri_f", Some("rr_1")),
            reply("ri_f", "rr_3"),
            // Another principal names the asker's root: never in its thread.
            item(&other, "ri_x", Some("ri_q")),
            // An unrelated root of the asker's.
            item(&asker, "ri_new", None),
        ];
        (
            events,
            crate::domain::models::ItemPrincipal::A2aPseudonym(asker),
        )
    }

    fn thread_ids(
        projection: &JournalRecipientItemProjection,
        principal: &crate::domain::models::ItemPrincipal,
        id: &str,
    ) -> Option<(String, Vec<String>)> {
        projection
            .thread(&ItemScope::exact(principal.clone()), id)
            .map(|(root, turns)| {
                (
                    root,
                    turns.iter().map(|turn| turn.id().to_owned()).collect(),
                )
            })
    }

    /// Story 19.18 `K20` — interleaved live `apply` of items and reply turns
    /// yields the same thread order as a cold `from_entries` refold, and live
    /// orders are unique across BOTH maps.
    ///
    /// `M43` → RED: take the next live order from items only — a reply
    /// applied after an item ties with the next item, so the orders are not
    /// unique (and a restart would reorder the thread).
    #[test]
    fn live_apply_order_matches_a_cold_refold_across_items_and_turns() {
        let (events, principal) = thread_fixture();
        let live = JournalRecipientItemProjection::default();
        for event in &events {
            live.apply(event);
        }
        let entries: Vec<JournalEntry> = events
            .iter()
            .enumerate()
            .map(|(index, event)| entry(index as u64 + 1, event.clone()))
            .collect();
        let cold = JournalRecipientItemProjection::from_entries(&entries);
        for id in ["ri_q", "rr_2", "ri_f", "rr_3", "ri_new"] {
            assert_eq!(
                thread_ids(&live, &principal, id),
                thread_ids(&cold, &principal, id),
                "{id}"
            );
        }
        let snapshot = live.snapshot();
        let mut orders: Vec<u64> = snapshot
            .items
            .values()
            .map(|item| item.journal_order)
            .chain(snapshot.replies.values().map(|turn| turn.journal_order))
            .collect();
        let total = orders.len();
        orders.sort_unstable();
        orders.dedup();
        assert_eq!(orders.len(), total, "one order over both maps (`Q3`)");
    }

    /// Story 19.18 `AC4(b)` — any member resolves to the root, the thread is
    /// the root's whole chain in fold order, and another principal's item is
    /// never in it — nor addressable at all (R21).
    ///
    /// `M09` (unit half) → RED: drop the principal filter — `ri_x` joins.
    #[test]
    fn a_thread_is_one_principals_chain_in_fold_order() {
        let (events, principal) = thread_fixture();
        let entries: Vec<JournalEntry> = events
            .iter()
            .enumerate()
            .map(|(index, event)| entry(index as u64 + 1, event.clone()))
            .collect();
        let projection = JournalRecipientItemProjection::from_entries(&entries);
        let expected = (
            "ri_q".to_owned(),
            ["ri_q", "rr_1", "rr_2", "ri_f", "rr_3"]
                .map(str::to_owned)
                .to_vec(),
        );
        for member in ["ri_q", "rr_1", "rr_2", "ri_f", "rr_3"] {
            assert_eq!(
                thread_ids(&projection, &principal, member),
                Some(expected.clone())
            );
        }
        assert_eq!(
            thread_ids(&projection, &principal, "ri_new"),
            Some(("ri_new".to_owned(), vec!["ri_new".to_owned()]))
        );
        assert_eq!(thread_ids(&projection, &principal, "ri_x"), None);
        assert_eq!(thread_ids(&projection, &principal, "ri_never"), None);
        assert_eq!(
            projection.reply_count(&ItemAddress::from_a2a_ingress(
                match &principal {
                    crate::domain::models::ItemPrincipal::A2aPseudonym(peer) => peer.clone(),
                    _ => unreachable!(),
                },
                ItemId::from_replay("ri_q"),
            )),
            2
        );
        // Story 19.18c `A8` — a reply turn's nearest item is the item it
        // answers; another principal's id resolves to nothing.
        assert_eq!(
            projection.nearest_item(&ItemScope::exact(principal.clone()), "rr_3"),
            Some(NearestItem {
                capability_hash: None
            })
        );
        assert_eq!(
            projection.nearest_item(&ItemScope::exact(principal), "ri_x"),
            None
        );
    }

    /// Story 19.18 `AC4(b)` — the chain walk is bounded and cycle-safe: two
    /// items naming each other (a journal no front door writes) terminate.
    #[test]
    fn a_reply_cycle_terminates() {
        let peer = PeerId::from_public_key(&[33; 32]).unwrap();
        let item = |id: &str, reply_to: &str| RoomEvent::RecipientItemReceived {
            address: ItemAddress::from_a2a_ingress(peer.clone(), ItemId::from_replay(id)),
            task: id.to_owned(),
            alias: None,
            message_type: crate::domain::models::SemanticMessageType::Unknown,
            content: id.to_owned(),
            in_reply_to: Some(reply_to.to_owned()),
            capability_hash: None,
            reply_capability_matched: false,
            authorship: crate::domain::models::Authorship::Unknown,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            stories: None,
            selected_for: None,
        };
        let projection = JournalRecipientItemProjection::from_entries(&[
            entry(1, item("ri_a", "ri_b")),
            entry(2, item("ri_b", "ri_a")),
        ]);
        let principal = crate::domain::models::ItemPrincipal::A2aPseudonym(peer);
        assert!(
            projection
                .thread(&ItemScope::exact(principal), "ri_a")
                .is_some()
        );
    }
}
