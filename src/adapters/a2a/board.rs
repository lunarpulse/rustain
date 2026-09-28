//! Act 1's distribution board — the sender's view of a fan-out (Story 19.16b,
//! `AC3`; `UX-DR-TM-02`, ratified 2026-09-14).
//!
//! # The board is assembled from REMOTE reads, never from the local projection
//!
//! A recipient item exists **only on the host that received the message**: the
//! sole production producer of `RoomEvent::RecipientItemReceived` is the
//! inbound `message/send` path in [`super::server`]. Story 19.17's sender
//! delivery path reads `RECIPIENT_ITEM_METADATA_KEY` on that request's first
//! answer to classify `delivered`, but does not retain the item ID. ⛔ Rendering
//! this host's own `JournalRecipientItemProjection` would show its **inbound**
//! items as if they were its own outbound fan-out status — the board inverted
//! (`A21`).
//!
//! ⇒ One [`super::ITEMS_LIST_METHOD`] call per **configured A2A roster** peer,
//! aggregated here. ⛔ The peer set is A2A-roster-only: resolving `peer_id`
//! against `p2p.json` is refused by `DF-18-4d-J10-TRANSPORT-POLICY`. Row order
//! is `A2aDelegationRuntime::known_peer_ids`' own sort, so it is stable across
//! runs and across hosts without this module minting an ordering helper.
//!
//! # The correlation (`AC3(g)`): rows answer for THIS sender's dispatches
//!
//! The read verb's set is **principal**-scoped, and under a collapsed
//! principal (a loopback bind) it mixes every local caller's items. The
//! board therefore correlates each item's `task` against the sender's own
//! durable dispatch ledger (`RemoteEnvelopeDispatched` rows, keyed by the
//! peer's resolved identity): a row reports the newest item **this host
//! dispatched to that peer**, not the peer's newest bag. That inverse mapping
//! — item.task ↔ dispatched task — is the correlator's first production reader.
//! The `x-rustain-item-id` metadata carrier (`a2a/mod.rs:9`) also has Story
//! 19.17's first-answer reader; the board keeps its remote-list reader, while
//! Stories 19.18/19.21 own reply and scene consumption.
//!
//! # What this module does NOT do
//!
//! ⛔ No aggregate (`UX-DR-TM-02:217`): no `5 / 7`, no percentage, no single
//! value standing for the set. ⛔ No catch-all token (`:254`). ⛔ The header
//! counts exactly what it renders — **rows** — and deliberately does NOT
//! claim the addressed-recipient set (`:252`): `A21` proved the sender's
//! fan-out is not locally knowable, and an outcome-derived number wearing
//! that label would be a lie whichever way it drifted; Story 19.21's scene,
//! the only layer that knows the fan-out, owns the addressed count.
//! ⛔ Story 19.21 composes the Act-1 scene; this is the read, the aggregation
//! and one operator render.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use futures::future::join_all;

use super::client::CardSlot;
use super::driver::{A2aDelegationRuntime, TaskClient};
use super::endpoint::resolve_jsonrpc_endpoint;
use super::lifecycle::PollConfig;

/// One recipient's own outcome, from `UX-DR-TM-02`'s ratified state set.
///
/// ⛔ Four outcomes, four distinct marks, **no catch-all** (`:254`: *"No
/// `other`, no `unknown`, no shared glyph"*). ⛔ `pending send` is deliberately
/// absent: it is the *absence* of an outcome, not a fifth kind of one, and a
/// board assembled by polling cannot observe a send still in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardOutcome {
    /// The peer at this alias reported a deliberate human acknowledgement.
    Acknowledged,
    /// The item exists on the recipient's side and carries no acknowledgement
    /// yet. ⚠ Also where a **removed** item renders when nothing was
    /// acknowledged first: a tombstone keeps the item (`AD-1821`: *"the id
    /// stays claimed"*), and `:234` scopes `delivered` to *"the item exists on
    /// the recipient's side"* — which is exactly why `AD-1822` demanded a
    /// tombstone distinct from not-found.
    Delivered,
    /// The recipient's policy or the recipient refused the item.
    ///
    /// ⚠ Story 19.17 `G2(d)` produces this at **send time** from a first-answer
    /// rejection. A board read still produces no decline: the FR169 verdict
    /// remains Story 19.18's new item for this board to render. ⛔ Do not
    /// invent another producer.
    Declined,
    /// The read did not land. ⛔ Never `Declined`: *"a host being down is not a
    /// person saying no"* (`:236`).
    Unreachable,
}

impl BoardOutcome {
    /// The shipped mark, at its shipped meaning (`UX-DR-TM-02:241-246`).
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Acknowledged => "✓",
            Self::Delivered => "●",
            Self::Declined => "✗",
            Self::Unreachable => "⚠",
        }
    }

    /// The ratified row word.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Acknowledged => "acknowledged",
            Self::Delivered => "delivered",
            Self::Declined => "declined",
            Self::Unreachable => "unreachable",
        }
    }
}

/// One row: the roster alias and that recipient's own outcome.
///
/// `peer` is the roster alias (`A2aPeerSpec::id`), never a person-shaped name —
/// `✓ jun-dev`, never `✓ Jun` (`UX-DR-TM-02:202`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardRow {
    pub peer: String,
    pub outcome: BoardOutcome,
    /// The per-item picker rows beneath the outcome row (Story 19.16f `AC1`):
    /// every **correlated** item this peer listed, in arrival order. A detail
    /// level beneath the outcome — ⛔ never a fifth outcome token. Empty for an
    /// unreachable peer and for a peer holding nothing of ours.
    pub items: Vec<BoardItem>,
}

/// One item on the peer's host, as the peer listed it (Story 19.16f `AC1`).
///
/// Exists so the operator can **copy an item id** into `/team retract`: the
/// id is host-minted on the recipient and is otherwise reachable nowhere on
/// the sender. Fields hold the wire values raw; the render is the single
/// sanitize point (`AD-1824`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardItem {
    /// The recipient-minted `itemId`. ⛔ Peer text.
    pub item_id: String,
    /// The item's `task` — the sender's own `messageId` round-tripped, but a
    /// peer that returns a different string makes it peer text.
    pub task: Option<String>,
    pub state: BoardItemState,
    /// The server's **0-based** arrival index (`ordinal`).
    pub ordinal: u64,
    /// When the sender's retract was recorded on the recipient's host
    /// (Story 19.16d) — the first sender-side reader of `retractedAtMs`.
    pub retracted_at_ms: Option<i64>,
}

/// An item's state word, from the closed set the read verb serves. ⛔ Matched,
/// never echoed: an unnameable state withholds the item row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardItemState {
    Received,
    Acknowledged,
    Removed,
}

impl BoardItemState {
    /// Parse the wire word, or `None` for a state this build cannot name.
    #[must_use]
    pub fn from_wire(word: &str) -> Option<Self> {
        match word {
            "received" => Some(Self::Received),
            "acknowledged" => Some(Self::Acknowledged),
            "removed" => Some(Self::Removed),
            _ => None,
        }
    }

    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Acknowledged => "acknowledged",
            Self::Removed => "removed",
        }
    }
}

/// Most per-item rows rendered under one peer in-chat (Story 19.16f `AC1`).
///
/// A separate constant from `team_command::MAX_INCHAT_ROWS` (20): a log is
/// read, a picker is **searched** for one id. "Most recent" = highest
/// `ordinal`. The cut is stated, and its overflow line names the one surface
/// that lifts it — `/team board <peer-id>`, which renders one peer uncapped.
/// Lives beside the render that applies it: this module is the a2a adapter's
/// renderer and imports nothing from the TUI.
pub const MAX_INCHAT_ITEM_ROWS: usize = 60;

/// The assembled board.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardView {
    pub rows: Vec<BoardRow>,
    /// At least one **row-producing** peer answered over a bind where every
    /// local caller is the same principal (Story 19.16b `AC2`). Fail-closed on
    /// authority, fail-loud on legibility: the read **serves**, and the board
    /// **discloses** that the per-recipient attribution below cannot be
    /// separated by caller. ⚠ Accumulated only from replies that produced a
    /// row (19.16b review): a collapsed peer that contributed none must not
    /// taint the attribution of rows that did.
    pub principal_collapsed: bool,
    /// Recipients whose newest item carries a state this build cannot name.
    ///
    /// A newer peer may fold a state this enum does not have. ⛔ Widening a
    /// token to absorb it is what `:254` forbids, so the row is withheld and
    /// the count is **stated** — the version-skew *legibility* class
    /// `DF-19-15-RAP-DOMAIN-UNBUMPED` names, owned by row `19-16e`.
    pub unnameable: usize,
    /// How many roster peers the collect actually consulted (19.16b review).
    /// Distinguishes "every peer answered with nothing of ours" from "no peer
    /// is configured at all" — two states an operator must not have to guess
    /// apart.
    pub peers_consulted: usize,
    /// `true` when the board was narrowed to one roster peer
    /// (`/team board <peer-id>`, Story 19.16f `AC9`): its item rows render
    /// **uncapped** — the escape hatch the capped overflow line names.
    pub narrowed: bool,
}

/// The board's stated poll floor, refused when a refresh arrives inside it.
///
/// Story 19.16b `AC5(d)` — ⛔ **a design bound, never an NFR64 latency claim,
/// and never tested as one** (`amendment:163`; `prd.md:2604`'s 2 s is **not**
/// re-armed). The number is not invented: it is the shipped substrate's own
/// [`PollConfig::default`] interval, the same bound that stops an inbound
/// status flood running unbounded.
#[must_use]
pub fn board_refresh_floor() -> Duration {
    PollConfig::default().interval
}

/// Why a refresh was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshTooSoon {
    /// How much of the stated floor is left.
    pub remaining: Duration,
}

/// Assemble the board by reading every configured roster peer once.
///
/// Returns `Err` when the refresh arrives inside the stated floor
/// ([`board_refresh_floor`]) **or** while a previous collect is still in
/// flight (19.16b review): the caller renders the refusal rather than issuing
/// N remote reads a design bound says are too close together, or stacking a
/// second fan-out whose notices would land after a newer board's.
///
/// The per-peer reads run **concurrently** (19.16b review): the roster is the
/// sender's own configuration, the reads are independent, and a sequential
/// loop would let one black-holed host (30 s client timeout) stall every peer
/// sorted after it. Results fold back in roster order, so row order stays
/// `known_peer_ids`' own stable sort.
pub async fn collect_board(
    runtime: &A2aDelegationRuntime,
    now: Instant,
) -> Result<BoardView, RefreshTooSoon> {
    collect(runtime, runtime.known_peer_ids(), false, now).await
}

/// Why a narrowed board (`/team board <peer-id>`) was not collected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerBoardRefusal {
    /// The same floor / in-flight refusal the full board renders.
    TooSoon(RefreshTooSoon),
    /// `<peer-id>` names no configured A2A roster peer. ⛔ Decided before the
    /// guards: no network call, and the refresh floor is not consumed.
    UnknownPeer { peer: String, known: Vec<String> },
}

/// Assemble the board for **one** roster peer, its item rows uncapped
/// (Story 19.16f `AC9`, owner gate item 3 = C) — the escape hatch the capped
/// board's overflow line names. Same guards as [`collect_board`]: one
/// narrowed read is still a board refresh.
pub async fn collect_peer_board(
    runtime: &A2aDelegationRuntime,
    peer_id: &str,
    now: Instant,
) -> Result<BoardView, PeerBoardRefusal> {
    let known = runtime.known_peer_ids();
    if !known.iter().any(|known| known == peer_id) {
        return Err(PeerBoardRefusal::UnknownPeer {
            peer: peer_id.to_owned(),
            known,
        });
    }
    collect(runtime, vec![peer_id.to_owned()], true, now)
        .await
        .map_err(PeerBoardRefusal::TooSoon)
}

async fn collect(
    runtime: &A2aDelegationRuntime,
    peers: Vec<String>,
    narrowed: bool,
    now: Instant,
) -> Result<BoardView, RefreshTooSoon> {
    // In-flight first, so a rate refusal never consumes the fan-out slot and
    // a stacked collect never starts.
    let _in_flight = runtime.begin_board_collect()?;
    runtime.admit_board_refresh(now, board_refresh_floor())?;

    let dispatched = runtime.dispatched_tasks_by_peer().await;
    let mut view = BoardView {
        peers_consulted: peers.len(),
        narrowed,
        ..BoardView::default()
    };
    let replies = join_all(
        peers
            .iter()
            .map(|peer| read_peer(runtime, peer, dispatched.as_ref())),
    )
    .await;
    for (peer, reply) in peers.iter().zip(replies) {
        match reply {
            Ok(reply) => match reply.outcome {
                Some(Some(outcome)) => {
                    // The disclosure accumulates only from row-producing
                    // replies: it scopes "the rows above", and a collapsed
                    // peer that contributed none must not taint them.
                    view.principal_collapsed |= reply.principal_collapsed;
                    view.rows.push(BoardRow {
                        peer: peer.clone(),
                        outcome,
                        items: reply.items,
                    });
                }
                // The peer answered and holds a state this build cannot name;
                // withhold the row, state the count.
                Some(None) => view.unnameable += 1,
                // The peer answered and holds nothing of ours: this sender
                // never addressed it, so it is not on the board.
                None => {}
            },
            Err(()) => view.rows.push(BoardRow {
                peer: peer.clone(),
                outcome: BoardOutcome::Unreachable,
                items: Vec::new(),
            }),
        }
    }
    // The last rendered view, so a landed retract can re-render the board
    // with its mark WITHOUT a second fan-out (Story 19.16f `AC10(d)`).
    runtime.remember_board(view.clone()).await;
    Ok(view)
}

/// What one peer reported. `outcome == None` means the peer holds no item of
/// ours; `Some(None)` means its newest item carries an unnameable state.
struct PeerReply {
    outcome: Option<Option<BoardOutcome>>,
    items: Vec<BoardItem>,
    principal_collapsed: bool,
}

/// One remote read. Every failure — no binding, no cached card, no endpoint, a
/// refused anchor, a transport error, a `-32601` from a build without the
/// verb, or a success whose shape the client cannot read — is the same
/// operator fact: **this read did not land**.
async fn read_peer(
    runtime: &A2aDelegationRuntime,
    peer: &str,
    dispatched: Option<&std::collections::HashMap<String, HashSet<String>>>,
) -> Result<PeerReply, ()> {
    let (spec, client) = runtime.peer_binding(peer).ok_or(())?;
    let CardSlot::Ready(card, _trust) = client.card_slot().await else {
        return Err(());
    };
    let endpoint = resolve_jsonrpc_endpoint(&card).map_err(|_| ())?;
    let transport = TaskClient::new(client, endpoint.url().to_owned());
    let result = transport.list_items().await.map_err(|_| ())?;
    // The dispatch ledger is keyed by the peer's resolved identity (pinned or
    // alias-pseudonym) — the same key `RemoteEnvelopeDispatched` records.
    let dispatched_for_peer = dispatched.map(|ledger| {
        ledger
            .get(spec.resolved_identity().as_str())
            .cloned()
            .unwrap_or_default()
    });
    Ok(PeerReply {
        outcome: match parse_peer_reply(&result, dispatched_for_peer.as_ref()) {
            Ok(outcome) => outcome,
            Err(()) => return Err(()),
        },
        items: parse_peer_items(&result, dispatched_for_peer.as_ref()),
        principal_collapsed: collapse_flag(&result),
    })
}

/// The legibility flag, failing SAFE: a peer that will not say whether it can
/// separate callers is **assumed collapsed** (19.16b review). Every other
/// unknown in this module withholds or states; an absent disclosure flag must
/// not be the one unknown that silently takes the permissive value and lets
/// the board render attribution the substrate may not support.
fn collapse_flag(result: &serde_json::Value) -> bool {
    result
        .get("principalCollapsed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// The recipient's current outcome: the newest **correlated** item's state.
///
/// A recipient can hold several items from one sender (one per send), and
/// under a collapsed principal it holds other locals' too. The row answers
/// *"where does my message stand with this recipient"*, so it reports the
/// newest item whose `task` this host durably dispatched (`AC3(g)`); when the
/// dispatch ledger is unreadable (`dispatched == None`) it degrades to the
/// newest item overall — the shipped behaviour — rather than report nothing.
/// ⛔ Never a rollup across items: that is the aggregate `FR164` forbids.
///
/// `Err(())` means the reply's shape was unreadable — no `items` array at
/// all. An empty array is `Ok(None)`: the peer answered and holds nothing of
/// ours. ⛔ The two are different operator facts and must not share a path
/// (19.16b review): a shapeless success is a read that did not land, not a
/// quiet empty set.
fn parse_peer_reply(
    result: &serde_json::Value,
    dispatched: Option<&HashSet<String>>,
) -> Result<Option<Option<BoardOutcome>>, ()> {
    let Some(items) = result.get("items").and_then(serde_json::Value::as_array) else {
        return Err(());
    };
    let candidates = correlated(items, dispatched);
    let Some(newest) = candidates.into_iter().max_by_key(|item| {
        item.get("ordinal")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    }) else {
        return Ok(None);
    };
    Ok(Some(
        match newest.get("state").and_then(serde_json::Value::as_str) {
            Some("acknowledged") => Some(BoardOutcome::Acknowledged),
            // A tombstone renders at its last sender-visible outcome (`A22`):
            // the acknowledgement the sender already saw, if there was one,
            // else the item's bare existence.
            Some("removed")
                if newest
                    .get("acknowledgedBeforeRemoval")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true) =>
            {
                Some(BoardOutcome::Acknowledged)
            }
            Some("received" | "removed") => Some(BoardOutcome::Delivered),
            _ => None,
        },
    ))
}

/// The items this sender dispatched (`AC3(g)`): an item whose `task` is in
/// the sender's own durable dispatch ledger. `dispatched == None` (an
/// unreadable ledger) degrades to every item — the shipped degrade, never
/// nothing. Shared by the outcome row and the per-item rows, so the two can
/// never answer for different sets.
fn correlated<'a>(
    items: &'a [serde_json::Value],
    dispatched: Option<&HashSet<String>>,
) -> Vec<&'a serde_json::Value> {
    match dispatched {
        Some(dispatched) => items
            .iter()
            .filter(|item| {
                item.get("task")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|task| dispatched.contains(task))
            })
            .collect(),
        None => items.iter().collect(),
    }
}

/// The per-item picker rows (Story 19.16f `AC1`): every **correlated** item
/// the peer listed, in arrival order. The first production reader of the
/// wire's `itemId` and of 19.16d's `retractedAtMs`.
///
/// ⚠ An item with no string `itemId`, or whose state this build cannot name,
/// is withheld: a row the operator cannot act on, or one that would widen the
/// closed state set, is not a picker row.
fn parse_peer_items(
    result: &serde_json::Value,
    dispatched: Option<&HashSet<String>>,
) -> Vec<BoardItem> {
    let Some(items) = result.get("items").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut rows: Vec<BoardItem> = correlated(items, dispatched)
        .into_iter()
        .filter_map(|item| {
            Some(BoardItem {
                item_id: item.get("itemId")?.as_str()?.to_owned(),
                task: item
                    .get("task")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                state: BoardItemState::from_wire(item.get("state")?.as_str()?)?,
                ordinal: item
                    .get("ordinal")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                // Absent (not null) on the list when unretracted.
                retracted_at_ms: item
                    .get("retractedAtMs")
                    .and_then(serde_json::Value::as_i64),
            })
        })
        .collect();
    rows.sort_by_key(|item| item.ordinal);
    rows
}

/// `HH:MM` of a retract mark, in UTC, computed from the time-of-day rather
/// than sliced from a rendered year whose width is not bounded.
#[must_use]
pub fn mark_clock(retracted_at_ms: i64) -> String {
    let seconds = retracted_at_ms.div_euclid(1_000).rem_euclid(86_400);
    format!("{:02}:{:02}", seconds / 3_600, (seconds % 3_600) / 60)
}

/// One picker row, in the ratified grammar (`DESIGN.md` `board-item-row`):
/// `    item {id} · arrival {n} · task {task} · {state}[ · [retracted HH:MM]]`.
///
/// 🔴 The single sanitize point for peer text on this path (`AD-1824`,
/// Story 19.16f `F12`): the in-chat block now keeps `'\n'` as structure, so an
/// `itemId` or `task` carrying a newline would forge a second picker row.
/// `sanitize_disclosable` drops every control character and bounds the field
/// with a visible `…[truncated]` marker.
#[must_use]
pub fn render_item_row(item: &BoardItem) -> String {
    use crate::domain::services::transparency::{MAX_PEER_ID_BYTES, sanitize_disclosable};

    // `ordinal` is the server's 0-based arrival index; the ratified grammar
    // counts arrivals from 1.
    let mut row = format!(
        "    item {} · arrival {}",
        sanitize_disclosable(&item.item_id, MAX_PEER_ID_BYTES),
        item.ordinal.saturating_add(1)
    );
    if let Some(task) = &item.task {
        row.push_str(" · task ");
        row.push_str(&sanitize_disclosable(task, MAX_PEER_ID_BYTES));
    }
    row.push_str(" · ");
    row.push_str(item.state.word());
    if let Some(ms) = item.retracted_at_ms {
        row.push_str(&format!(" · [retracted {}]", mark_clock(ms)));
    }
    row
}

/// The item rows beneath one outcome row. Capped at
/// [`MAX_INCHAT_ITEM_ROWS`] most recent unless the board is narrowed to this
/// one peer; the cut is stated, and `{M}` is the number of correlated items
/// for this peer BEFORE the cap (never the rendered count, which would read
/// "the 60 most recent of 60").
fn render_item_rows(out: &mut String, row: &BoardRow, narrowed: bool) {
    let total = row.items.len();
    let skipped = if narrowed {
        0
    } else {
        total.saturating_sub(MAX_INCHAT_ITEM_ROWS)
    };
    if skipped > 0 {
        out.push_str(&format!(
            "    · showing the {MAX_INCHAT_ITEM_ROWS} most recent of {total} items for this \
             peer — narrow with '/team board <peer-id>'\n"
        ));
    }
    for item in row.items.iter().skip(skipped) {
        out.push_str(&render_item_row(item));
        out.push('\n');
    }
}

/// Apply a landed retract's mark to one item of a remembered view (Story
/// 19.16f `AC10(d)`). Returns whether an item matched.
pub(crate) fn apply_retract_mark(
    view: &mut BoardView,
    peer: &str,
    item_id: &str,
    retracted_at_ms: i64,
) -> bool {
    view.rows
        .iter_mut()
        .filter(|row| row.peer == peer)
        .flat_map(|row| row.items.iter_mut())
        .find(|item| item.item_id == item_id)
        .map(|item| {
            item.retracted_at_ms.get_or_insert(retracted_at_ms);
        })
        .is_some()
}

/// Rendered when `/team board <peer-id>` names no roster peer.
#[must_use]
pub fn render_peer_board_refusal(refusal: &PeerBoardRefusal) -> String {
    match refusal {
        PeerBoardRefusal::TooSoon(refusal) => render_refresh_refusal(*refusal),
        PeerBoardRefusal::UnknownPeer { peer, known } => format!(
            "· no configured A2A peer is named '{}' — configured: {}.",
            crate::domain::services::transparency::sanitize_disclosable(
                peer,
                crate::domain::services::transparency::MAX_PEER_ID_BYTES
            ),
            if known.is_empty() {
                "none".to_owned()
            } else {
                known.join(", ")
            }
        ),
    }
}

/// The one operator render. Story 19.21 composes the Act-1 scene; this is the
/// row grammar, in `UX-DR-TM-02`'s ratified tokens and nothing else.
#[must_use]
pub fn render_board(view: &BoardView) -> String {
    if view.peers_consulted == 0 {
        // ⛔ Not the "holds an item" sentence: that claim is about peers that
        // exist. A first-run operator with no roster must be told the board
        // has nothing to read, not that their fan-out landed nowhere
        // (19.16b review).
        return "· no A2A peer is configured for this session, so there is no board to read."
            .to_owned();
    }
    if view.rows.is_empty() && view.unnameable == 0 {
        let mut out = "· no configured A2A peer holds an item from this sender.".to_owned();
        if view.principal_collapsed {
            out.push('\n');
            out.push_str(COLLAPSED_PRINCIPAL_DISCLOSURE);
        }
        return out;
    }
    // The header counts ROWS — exactly what is rendered (19.16b review ruling
    // R1, 5–0). ⛔ It must not claim the addressed-recipient set: that number
    // is not computable at this layer (`A21`), and it is NOT a completion
    // aggregate either (`:252`); Story 19.21's scene owns the addressed count.
    let mut out = format!(
        "Act 1 distribution board · {} row{}\n",
        view.rows.len(),
        if view.rows.len() == 1 { "" } else { "s" }
    );
    let width = view
        .rows
        .iter()
        .map(|row| row.peer.chars().count())
        .max()
        .unwrap_or(0);
    for row in &view.rows {
        out.push_str(&format!(
            "  {} {:width$}  {}\n",
            row.outcome.token(),
            row.peer,
            row.outcome.label(),
        ));
        render_item_rows(&mut out, row, view.narrowed);
    }
    if view.unnameable > 0 {
        out.push_str(&format!(
            "· {} roster peer{} reported an outcome this build cannot name — the peer runs a \
             newer build (A2A version boundary: row `19-16e`).\n",
            view.unnameable,
            if view.unnameable == 1 { "" } else { "s" }
        ));
    }
    if view.principal_collapsed {
        out.push_str(COLLAPSED_PRINCIPAL_DISCLOSURE);
        out.push('\n');
    }
    out
}

/// Rendered whenever a peer reported that it could not tell this caller apart
/// from any other local one.
///
/// ⛔ A **legibility** statement, not a security claim: it inherits the wording
/// ceiling (`amendment:311`), so a cross-host item is never described as
/// authentic, tamper-evident, cryptographically attested, or an audit trail.
/// ⛔ It renders on the board, never only in a log — an operator must never be
/// shown a per-peer attribution the substrate cannot support.
pub const COLLAPSED_PRINCIPAL_DISCLOSURE: &str = "⚠ At least one peer answered over a loopback bind, where every local caller is the same \
     principal — the rows above are that host's items for all local callers, not this caller's \
     alone.";

/// Rendered when a refresh arrives inside the stated poll floor.
#[must_use]
pub fn render_refresh_refusal(refusal: RefreshTooSoon) -> String {
    format!(
        "· the board's stated refresh floor is {} ms; {} ms left. ⛔ A design bound on how often \
         this host polls its peers — not a latency guarantee about how fast an acknowledgement \
         arrives.",
        board_refresh_floor().as_millis(),
        refusal.remaining.as_millis().max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Story 19.16b AC3(e) — a `removed` row renders at the last sender-visible
    /// outcome, ⛔ never through the not-found path's `⚠ unreachable`.
    ///
    /// **Mutant → RED:** map `"removed"` to `BoardOutcome::Unreachable`.
    /// **Mutant → RED:** drop the `acknowledgedBeforeRemoval` arm — the
    /// removed-after-ack row falls to `● delivered` and the acknowledgement
    /// the operator already saw disappears (`A22`).
    #[test]
    fn a_removed_item_renders_at_its_last_sender_visible_outcome() {
        let removed_bare = serde_json::json!({
            "items": [{ "itemId": "ri_a", "task": "t", "state": "removed", "ordinal": 0 }],
        });
        assert_eq!(
            parse_peer_reply(&removed_bare, None),
            Ok(Some(Some(BoardOutcome::Delivered))),
            "a tombstone keeps the item, so `delivered` stays literally true; \
             `unreachable` would claim the send never landed"
        );
        let removed_after_ack = serde_json::json!({
            "items": [{
                "itemId": "ri_a", "task": "t", "state": "removed",
                "acknowledgedBeforeRemoval": true, "ordinal": 0,
            }],
        });
        assert_eq!(
            parse_peer_reply(&removed_after_ack, None),
            Ok(Some(Some(BoardOutcome::Acknowledged))),
            "the acknowledgement the sender already saw is the last sender-visible \
             outcome — removal is the recipient's own housekeeping (`A22`)"
        );
        // Positive control: the mapper is not a constant function.
        let acknowledged = serde_json::json!({
            "items": [{ "itemId": "ri_a", "task": "t", "state": "acknowledged", "ordinal": 0 }],
        });
        assert_eq!(
            parse_peer_reply(&acknowledged, None),
            Ok(Some(Some(BoardOutcome::Acknowledged)))
        );
    }

    /// Story 19.16b AC3(g) — the row answers for **this sender's** dispatches,
    /// not the peer's newest bag: under a collapsed principal the peer's set
    /// mixes every local caller's items, and the task correlation is what
    /// keeps the attribution truthful.
    ///
    /// **Mutant → RED:** drop the correlation filter — the NEWEST item
    /// overall (someone else's) wins and the row reports a foreign send.
    #[test]
    fn the_row_answers_for_this_senders_dispatches_not_the_peers_newest_bag() {
        let ours = vec!["task-ours".to_owned()]
            .into_iter()
            .collect::<HashSet<_>>();
        let reply = serde_json::json!({
            "items": [
                { "itemId": "ri_ours",  "task": "task-ours",   "state": "received",     "ordinal": 0 },
                { "itemId": "ri_theirs", "task": "task-theirs", "state": "acknowledged", "ordinal": 7 },
            ],
        });
        assert_eq!(
            parse_peer_reply(&reply, Some(&ours)),
            Ok(Some(Some(BoardOutcome::Delivered))),
            "the newest item overall is someone else's acknowledged send; ours is \
             the newest item THIS host dispatched"
        );
        // Positive control: with the ledger unreadable the board degrades to
        // the uncorrelated newest — the shipped behaviour — never to nothing.
        assert_eq!(
            parse_peer_reply(&reply, None),
            Ok(Some(Some(BoardOutcome::Acknowledged)))
        );
    }

    /// Story 19.16b review — a shapeless success is a read that did not land,
    /// ⛔ not a quiet empty set: `items` missing or non-array must be `Err`
    /// (the caller renders `⚠`), while an EMPTY array is `Ok(None)` (no row).
    ///
    /// **Mutant → RED:** collapse both into `None`.
    #[test]
    fn a_shapeless_success_is_not_a_quiet_empty_set() {
        for shapeless in [
            serde_json::json!({}),
            serde_json::json!({ "items": null }),
            serde_json::json!({ "items": 7 }),
        ] {
            assert!(
                parse_peer_reply(&shapeless, None).is_err(),
                "a reply the client cannot read is an unread read: {shapeless}"
            );
        }
        assert_eq!(
            parse_peer_reply(&serde_json::json!({ "items": [] }), None),
            Ok(None),
            "an empty set is a real answer: this sender never addressed the peer"
        );
    }

    /// Story 19.16b AC2(c) — an absent or non-boolean collapse flag fails
    /// SAFE: the peer that will not say is assumed collapsed.
    ///
    /// **Mutant → RED:** `unwrap_or(false)`.
    #[test]
    fn an_absent_collapse_flag_defaults_to_collapsed() {
        assert!(collapse_flag(
            &serde_json::json!({ "principalCollapsed": true })
        ));
        assert!(!collapse_flag(
            &serde_json::json!({ "principalCollapsed": false })
        ));
        assert!(
            collapse_flag(&serde_json::json!({})),
            "silence must never read as separated attribution"
        );
        assert!(
            collapse_flag(&serde_json::json!({ "principalCollapsed": "true" })),
            "a stringified flag is an unknown, and unknowns fail loud here"
        );
    }

    /// Story 19.16b review — an empty roster says THERE IS NO BOARD, ⛔ not
    /// that every peer holds nothing.
    ///
    /// **Mutant → RED:** render the "holds an item" sentence for
    /// `peers_consulted == 0`.
    #[test]
    fn an_empty_roster_is_not_an_empty_board() {
        let rendered = render_board(&BoardView::default());
        assert!(
            rendered.contains("no A2A peer is configured"),
            "a first-run operator must learn the roster is empty, not that their \
             fan-out landed nowhere: {rendered}"
        );
        assert!(
            !rendered.contains("holds an item"),
            "that claim is about peers that exist: {rendered}"
        );
    }

    /// Story 19.16b AC3(c) — a state this build cannot name withholds its row
    /// rather than widening a token (`UX-DR-TM-02:254`).
    ///
    /// **Mutant → RED:** fall back to `BoardOutcome::Delivered` for an unknown
    /// state.
    #[test]
    fn an_unnameable_wire_state_withholds_its_row_instead_of_widening_a_token() {
        let future = serde_json::json!({
            "items": [{ "itemId": "ri_a", "task": "t", "state": "escalated", "ordinal": 0 }],
        });
        assert_eq!(parse_peer_reply(&future, None), Ok(Some(None)));

        let view = BoardView {
            rows: Vec::new(),
            principal_collapsed: false,
            unnameable: 1,
            peers_consulted: 1,
            narrowed: false,
        };
        let rendered = render_board(&view);
        assert!(
            rendered.contains("cannot name"),
            "the withheld row must be STATED, never silently dropped: {rendered}"
        );
        for token in ["✓", "●", "✗", "⚠"] {
            assert!(
                !rendered.contains(token),
                "an unnameable outcome must not borrow a ratified token: {rendered}"
            );
        }
    }

    /// Story 19.16b AC3(d) — ⛔ aggregates are forbidden (`UX-DR-TM-02:217`),
    /// and the review's ruling R1 (5–0): the header counts ROWS, ⛔ never
    /// "recipients" — an outcome-derived number wearing the addressed-set
    /// label is the lie `A21` makes unimplementable.
    ///
    /// Positive control is REQUIRED here, or the assertion is green from birth:
    /// the same render must carry the per-recipient rows and the row count.
    ///
    /// **Mutant → RED:** append a completion count to the header, or restore
    /// the word "recipients" over the outcome-derived count.
    #[test]
    fn the_board_renders_rows_and_a_row_count_but_never_an_aggregate_or_a_recipient_claim() {
        let view = BoardView {
            rows: vec![
                BoardRow {
                    peer: "jun-dev".to_owned(),
                    outcome: BoardOutcome::Acknowledged,
                    items: Vec::new(),
                },
                BoardRow {
                    peer: "tom-dev".to_owned(),
                    outcome: BoardOutcome::Delivered,
                    items: Vec::new(),
                },
                BoardRow {
                    peer: "nina-ux".to_owned(),
                    outcome: BoardOutcome::Unreachable,
                    items: Vec::new(),
                },
            ],
            principal_collapsed: false,
            unnameable: 0,
            peers_consulted: 3,
            narrowed: false,
        };
        let rendered = render_board(&view);

        // Positive control — the rows and the row count DO render.
        assert!(rendered.contains("3 rows"), "{rendered}");
        assert!(rendered.contains("✓ jun-dev"), "{rendered}");
        assert!(rendered.contains("● tom-dev"), "{rendered}");
        assert!(rendered.contains("⚠ nina-ux"), "{rendered}");
        assert!(rendered.contains("unreachable"), "{rendered}");

        // The boundary: no value stands for the set.
        for aggregate in ["1 / 3", "1 of 3", "33%", "1/3"] {
            assert!(
                !rendered.contains(aggregate),
                "the board is N independent facts, not a completion meter: {rendered}"
            );
        }

        // R1's wording mutant: with a peer withheld (it demonstrably answered),
        // the header must not claim "recipients" — that count would read as
        // the addressed set and be wrong in front of its own fixture.
        let withheld = BoardView {
            rows: Vec::new(),
            principal_collapsed: false,
            unnameable: 1,
            peers_consulted: 1,
            narrowed: false,
        };
        let withheld_render = render_board(&withheld);
        assert!(
            !withheld_render.contains("recipient"),
            "an outcome-derived count must not wear the addressed-set label: {withheld_render}"
        );
    }

    /// Story 19.16b AC2(c) — the collapse DISCLOSES on the board, and the
    /// disclosure is conditional on the measured collapse. ⚠ It renders on
    /// the empty-board early path too (19.16b review): a measured collapse is
    /// never silently discarded.
    ///
    /// **Mutant → RED:** drop the disclosure, or print it unconditionally.
    #[test]
    fn the_collapsed_principal_disclosure_renders_only_when_a_peer_reported_it() {
        let row = BoardRow {
            peer: "jun-dev".to_owned(),
            outcome: BoardOutcome::Acknowledged,
            items: Vec::new(),
        };
        let collapsed = render_board(&BoardView {
            rows: vec![row.clone()],
            principal_collapsed: true,
            unnameable: 0,
            peers_consulted: 1,
            narrowed: false,
        });
        assert!(
            collapsed.contains(COLLAPSED_PRINCIPAL_DISCLOSURE),
            "{collapsed}"
        );

        let separated = render_board(&BoardView {
            rows: vec![row],
            principal_collapsed: false,
            unnameable: 0,
            peers_consulted: 1,
            narrowed: false,
        });
        assert!(
            !separated.contains(COLLAPSED_PRINCIPAL_DISCLOSURE),
            "off loopback the credential-derived principal IS the boundary, so the \
             disclosure would be false: {separated}"
        );

        let collapsed_empty = render_board(&BoardView {
            rows: Vec::new(),
            principal_collapsed: true,
            unnameable: 0,
            peers_consulted: 1,
            narrowed: false,
        });
        assert!(
            collapsed_empty.contains(COLLAPSED_PRINCIPAL_DISCLOSURE),
            "a measured collapse survives the empty-board path: {collapsed_empty}"
        );
    }

    /// Story 19.16b AC3(b2) — an unreachable peer is `⚠`, ⛔ never `✗ declined`.
    /// The production mapping site is `collect_board`'s `Err(())` arm; its
    /// keystone lives in `tests/a2a_server.rs`
    /// (`ac3b2_an_unreachable_roster_peer_renders_warned_never_declined`).
    /// This unit pins the token table the render consumes.
    #[test]
    fn an_unreachable_peer_is_warned_about_and_never_reported_as_a_refusal() {
        assert_eq!(BoardOutcome::Unreachable.token(), "⚠");
        assert_eq!(BoardOutcome::Unreachable.label(), "unreachable");
        assert_ne!(
            BoardOutcome::Unreachable.token(),
            BoardOutcome::Declined.token(),
            "a host being down is not a person saying no"
        );
        let rendered = render_board(&BoardView {
            rows: vec![BoardRow {
                peer: "nina-ux".to_owned(),
                outcome: BoardOutcome::Unreachable,
                items: Vec::new(),
            }],
            principal_collapsed: false,
            unnameable: 0,
            peers_consulted: 1,
            narrowed: false,
        });
        assert!(rendered.contains("⚠ nina-ux"), "{rendered}");
        assert!(!rendered.contains("declined"), "{rendered}");
    }

    /// Story 19.16b AC3(c) — four outcomes, four distinct marks, no shared
    /// glyph (`UX-DR-TM-02:254`).
    #[test]
    fn every_board_outcome_carries_its_own_mark_and_its_own_word() {
        let all = [
            BoardOutcome::Acknowledged,
            BoardOutcome::Delivered,
            BoardOutcome::Declined,
            BoardOutcome::Unreachable,
        ];
        let mut tokens = all.map(BoardOutcome::token).to_vec();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), all.len(), "a shared glyph is a catch-all");
        let mut labels = all.map(BoardOutcome::label).to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), all.len());
    }

    // ── Story 19.16f · the per-item picker rows ─────────────────────────────

    fn view_with(items: Vec<BoardItem>) -> BoardView {
        BoardView {
            rows: vec![BoardRow {
                peer: "jun-dev".to_owned(),
                outcome: BoardOutcome::Delivered,
                items,
            }],
            principal_collapsed: false,
            unnameable: 0,
            peers_consulted: 1,
            narrowed: false,
        }
    }

    fn item(item_id: &str, ordinal: u64) -> BoardItem {
        BoardItem {
            item_id: item_id.to_owned(),
            task: Some("task-7f2a".to_owned()),
            state: BoardItemState::Received,
            ordinal,
            retracted_at_ms: None,
        }
    }

    /// Story 19.16f AC1(a) — the item id is REACHABLE: the board carries a
    /// line an `itemId` can be copied from, in the ratified grammar, with the
    /// 0-based `ordinal` rendered as `arrival 1`.
    ///
    /// **Mutant `M01` → RED:** render the outcome row only. **Positive
    /// control:** a peer holding no correlated items renders its outcome row
    /// and no item row.
    #[test]
    fn the_board_carries_a_copyable_item_row_under_its_outcome_row() {
        let reply = serde_json::json!({
            "items": [{
                "itemId": "ri_V1St", "task": "task-7f2a", "state": "acknowledged",
                "ordinal": 0, "retractedAtMs": 1_700_000_000_000_i64,
            }],
        });
        let items = parse_peer_items(&reply, None);
        let rendered = render_board(&view_with(items));
        assert!(
            rendered.contains(
                "  ● jun-dev  delivered\n    item ri_V1St · arrival 1 · task task-7f2a · \
                 acknowledged · [retracted 22:13]\n"
            ),
            "{rendered}"
        );

        let empty = render_board(&view_with(Vec::new()));
        assert!(empty.contains("● jun-dev  delivered"), "{empty}");
        assert!(!empty.contains("    item "), "{empty}");
    }

    #[test]
    fn retract_clock_does_not_depend_on_the_rendered_year_width() {
        assert_eq!(mark_clock(253_402_300_800_000), "00:00");
        assert_eq!(mark_clock(-60_000), "23:59");
    }
    /// Story 19.16f AC1(a) — item rows answer for THIS sender's dispatches,
    /// through the same correlation as the outcome row.
    ///
    /// **Mutant `M18` → RED:** drop the correlation filter for item rows.
    /// **Positive control:** an unreadable ledger degrades to every item.
    #[test]
    fn item_rows_are_correlated_to_the_senders_own_dispatches() {
        let ours: HashSet<String> = std::iter::once("task-A".to_owned()).collect();
        let reply = serde_json::json!({
            "items": [
                { "itemId": "ri_a", "task": "task-A", "state": "received", "ordinal": 0 },
                { "itemId": "ri_b", "task": "task-B", "state": "received", "ordinal": 1 },
            ],
        });
        let correlated = parse_peer_items(&reply, Some(&ours));
        assert_eq!(
            correlated
                .iter()
                .map(|i| i.item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["ri_a"]
        );
        let degraded = parse_peer_items(&reply, None);
        assert_eq!(degraded.len(), 2, "the shipped degrade, never nothing");
    }

    /// Story 19.16f `F12` — every peer-sourced field is single-line sanitized
    /// at the render, because the in-chat block now keeps `'\n'` as structure.
    ///
    /// **Mutant `M19` → RED:** render `itemId` raw — the forged newline makes
    /// a second picker row, and a 10 KB id renders unbounded. **Positive
    /// control:** a clean id renders unchanged.
    #[test]
    fn a_peer_cannot_forge_a_picker_row_or_flood_the_block() {
        let forged = item(
            "ri_a\n    item ri_forged · arrival 9 · task x · received",
            0,
        );
        let rendered = render_board(&view_with(vec![forged]));
        let item_rows = rendered
            .lines()
            .filter(|line| line.starts_with("    item "))
            .count();
        assert_eq!(item_rows, 1, "{rendered}");

        let huge = render_item_row(&item(&"x".repeat(10 * 1024), 0));
        let id = huge
            .strip_prefix("    item ")
            .and_then(|rest| rest.split(" · arrival").next())
            .expect("row grammar");
        assert!(id.ends_with("…[truncated]"), "{id}");
        assert!(id.len() <= 256 + "…[truncated]".len(), "{}", id.len());

        assert_eq!(
            render_item_row(&item("ri_clean", 0)),
            "    item ri_clean · arrival 1 · task task-7f2a · received"
        );
    }

    /// Story 19.16f AC1(b) — the cap keeps the highest ordinals, states the
    /// cut with `{M}` = the correlated count BEFORE the cap, and a narrowed
    /// board lifts it.
    #[test]
    fn the_picker_cap_keeps_the_most_recent_and_states_the_pre_cap_count() {
        let items: Vec<BoardItem> = (0..61).map(|n| item(&format!("ri_{n:02}"), n)).collect();
        let capped = render_board(&view_with(items.clone()));
        assert!(
            capped.contains(
                "· showing the 60 most recent of 61 items for this peer — narrow with \
                 '/team board <peer-id>'"
            ),
            "{capped}"
        );
        assert!(!capped.contains("item ri_00 "), "{capped}");
        assert!(capped.contains("item ri_60 · arrival 61"), "{capped}");
        let mut narrowed = view_with(items);
        narrowed.narrowed = true;
        let narrowed = render_board(&narrowed);
        assert!(narrowed.contains("item ri_00 · arrival 1"), "{narrowed}");
        assert!(!narrowed.contains("most recent"), "{narrowed}");
    }

    /// Story 19.16f AC6(a) — ⛔ `M07` is `No mutant:` (`P10`): the retract
    /// ships zero lines of token mapping. Positive control (i): an item that
    /// carries `retractedAtMs` still renders its outcome as `● delivered` — a
    /// retract changes no outcome; the item still exists.
    #[test]
    fn a_retracted_item_keeps_its_outcome_token() {
        let reply = serde_json::json!({
            "items": [{
                "itemId": "ri_a", "task": "t", "state": "received", "ordinal": 0,
                "retractedAtMs": 1_700_000_000_000_i64,
            }],
        });
        assert_eq!(
            parse_peer_reply(&reply, None),
            Ok(Some(Some(BoardOutcome::Delivered)))
        );
    }
}
