//! Persistence fence carried by a [`Conversation`](super::Conversation) (D-24, D24-AD1).
//!
//! A conversation is loaded or created against one *base*: the incarnation (epoch) and
//! version a store last accepted for it. A plain `clone()` is another holder of the same live
//! lineage (the TUI copies conversations between tab and stage constantly). A *capture*
//! ([`Conversation::capture`](super::Conversation::capture)) is what crosses a spawn or an await
//! boundary: it binds an immutable [`Stamp`] (loaded base, per-writer operation id and, once
//! the store has armed the lineage, a cross-process capture sequence) to the bytes being
//! handed off, before anything can separate them from the live state. A store admits a stamp
//! under per-session authority and refuses anything stale. A stamp is never re-derived from a
//! newer base: capturing a capture copies the original stamp. A save of a conversation that
//! carries no stamp captures the live state at the call (borrowed authoritative state).
//!
//! The domain only holds the bookkeeping; the store that owns the authority implements
//! [`SequenceIssuer`] and interprets the stamps.

use std::fmt;
use std::sync::{Arc, Mutex};

/// Cross-process capture-sequence source for one physical session. Implemented by the store.
pub trait SequenceIssuer: Send + Sync + fmt::Debug {
    /// The next sequence for this session. Strictly increasing across every coordinating
    /// process; a gap (issued, never accepted) is harmless.
    fn issue(&self) -> Option<u64>;
}

/// What a conversation was loaded or last accepted against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Base {
    /// Observed missing: a new conversation that no store has accepted yet.
    Absent,
    /// Loaded from a session that predates the authority record (first upgraded open).
    Legacy,
    /// Accepted by the store at this incarnation and version.
    Accepted { epoch: String, version: u64 },
}

/// Immutable evidence bound to one capture.
#[derive(Clone, Debug)]
pub struct Stamp {
    /// Loaded base the captured bytes derive from.
    pub base: Base,
    /// Cross-process capture sequence; `None` until the store armed the lineage.
    pub seq: Option<u64>,
    /// Writer lineage (one per live conversation owner, never a PID).
    pub lineage: Arc<str>,
    /// Strictly increasing operation id inside [`Self::lineage`].
    pub op: u64,
    lineage_cell: Arc<Lineage>,
}

impl Stamp {
    /// Record the store's accepted result back on the lineage this stamp came from.
    pub fn settle(&self, base: Base, issuer: Option<Arc<dyn SequenceIssuer>>) {
        self.lineage_cell.settle(base, issuer);
    }
}

#[derive(Debug)]
struct LineageState {
    base: Base,
    next_op: u64,
    issuer: Option<Arc<dyn SequenceIssuer>>,
}

#[derive(Debug)]
struct Lineage {
    id: Arc<str>,
    state: Mutex<LineageState>,
}

impl Lineage {
    fn new(base: Base) -> Arc<Self> {
        Arc::new(Self {
            id: Arc::from(nanoid::nanoid!()),
            state: Mutex::new(LineageState {
                base,
                next_op: 1,
                issuer: None,
            }),
        })
    }

    fn stamp(self: &Arc<Self>) -> Stamp {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let op = state.next_op;
        state.next_op += 1;
        Stamp {
            base: state.base.clone(),
            seq: state.issuer.as_ref().and_then(|issuer| issuer.issue()),
            lineage: self.id.clone(),
            op,
            lineage_cell: self.clone(),
        }
    }

    fn settle_issuer(&self, issuer: Option<Arc<dyn SequenceIssuer>>) {
        if issuer.is_some() {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).issuer = issuer;
        }
    }

    fn settle(&self, base: Base, issuer: Option<Arc<dyn SequenceIssuer>>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let newer = match (&state.base, &base) {
            (
                Base::Accepted {
                    epoch: old_epoch,
                    version: old,
                },
                Base::Accepted { epoch, version },
            ) => old_epoch != epoch || version > old,
            _ => true,
        };
        if newer {
            state.base = base;
        }
        if issuer.is_some() {
            state.issuer = issuer;
        }
    }
}

/// The fence field of a conversation: its shared lineage plus, on a capture, the stamp bound
/// when it was captured.
pub struct PersistTag {
    lineage: Arc<Lineage>,
    stamp: Mutex<Option<Stamp>>,
}

impl PersistTag {
    /// A tag for a conversation loaded from a store at `base`.
    pub fn loaded(base: Base) -> Self {
        Self::loaded_armed(base, None)
    }

    /// A tag whose lineage already has the store's capture-sequence source.
    pub fn loaded_armed(base: Base, issuer: Option<Arc<dyn SequenceIssuer>>) -> Self {
        let lineage = Lineage::new(base);
        lineage.settle_issuer(issuer);
        Self {
            lineage,
            stamp: Mutex::new(None),
        }
    }

    /// The base this tag currently believes the store holds (the lineage's settled base).
    pub fn base(&self) -> Base {
        self.lineage
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .base
            .clone()
    }

    /// A tag for a capture: the same lineage with the stamp bound now (or this tag's own bound
    /// stamp, copied: an old capture is never re-stamped).
    pub fn bind(&self) -> Self {
        Self {
            lineage: self.lineage.clone(),
            stamp: Mutex::new(Some(self.capture())),
        }
    }

    /// The stamp for a write: a capture's bound stamp, or a fresh stamp of the live state.
    pub fn capture(&self) -> Stamp {
        match self
            .stamp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(bound) => bound.clone(),
            None => self.lineage.stamp(),
        }
    }

    /// Record an accepted operation: the lineage advances to `base`, and a clone's bound
    /// stamp becomes its own accepted successor (the one validated causal successor).
    pub fn settle(&self, stamp: &Stamp, base: Base, issuer: Option<Arc<dyn SequenceIssuer>>) {
        stamp.settle(base, issuer);
        self.advance(stamp);
    }

    /// After the lineage settled `accepted`'s operation: a capture bound to exactly that stamp
    /// takes its successor stamp. A capture bound to anything else keeps its evidence.
    pub fn advance(&self, accepted: &Stamp) {
        let mut bound = self.stamp.lock().unwrap_or_else(|e| e.into_inner());
        if bound
            .as_ref()
            .is_some_and(|b| b.op == accepted.op && b.lineage == accepted.lineage)
        {
            *bound = Some(self.lineage.stamp());
        }
    }
}

impl Default for PersistTag {
    fn default() -> Self {
        Self::loaded(Base::Absent)
    }
}

impl Clone for PersistTag {
    /// A plain clone is another holder of the same lineage; it keeps a bound stamp it was
    /// captured with and binds none otherwise.
    fn clone(&self) -> Self {
        Self {
            lineage: self.lineage.clone(),
            stamp: Mutex::new(self.stamp.lock().unwrap_or_else(|e| e.into_inner()).clone()),
        }
    }
}

impl fmt::Debug for PersistTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistTag")
            .field("base", &self.base())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Counter(std::sync::atomic::AtomicU64);
    impl SequenceIssuer for Counter {
        fn issue(&self) -> Option<u64> {
            Some(self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
        }
    }

    fn accepted(version: u64) -> Base {
        Base::Accepted {
            epoch: "e".into(),
            version,
        }
    }

    #[test]
    fn a_capture_binds_the_base_it_was_captured_at() {
        let live = PersistTag::loaded(accepted(1));
        let early = live.bind();
        live.settle(&live.capture(), accepted(2), None);
        let late = live.bind();
        assert_eq!(early.capture().base, accepted(1));
        assert_eq!(late.capture().base, accepted(2));
    }

    #[test]
    fn capturing_a_capture_copies_its_stamp_instead_of_restamping() {
        let live = PersistTag::loaded(accepted(1));
        let early = live.bind();
        live.settle(&live.capture(), accepted(2), None);
        let copy = early.bind();
        assert_eq!(copy.capture().base, accepted(1));
        assert_eq!(copy.capture().op, early.capture().op);
    }

    #[test]
    fn a_settle_never_moves_the_lineage_backwards() {
        let live = PersistTag::loaded(accepted(5));
        live.settle(&live.capture(), accepted(3), None);
        assert_eq!(live.base(), accepted(5));
    }

    #[test]
    fn an_armed_lineage_stamps_strictly_increasing_sequences() {
        let live = PersistTag::loaded(accepted(1));
        let issuer: Arc<dyn SequenceIssuer> = Arc::new(Counter(Default::default()));
        live.settle(&live.capture(), accepted(1), Some(issuer));
        let first = live.bind().capture().seq;
        let second = live.bind().capture().seq;
        assert!(first.is_some() && second > first);
    }

    #[test]
    fn a_bound_capture_advances_to_its_own_accepted_successor() {
        let live = PersistTag::loaded(accepted(1));
        let producer = live.bind();
        let first = producer.capture();
        producer.settle(&first, accepted(2), None);
        let next = producer.capture();
        assert_eq!(next.base, accepted(2));
        assert!(next.op > first.op);
        assert_eq!(live.base(), accepted(2));
    }

    #[test]
    fn a_plain_clone_is_a_holder_that_stamps_the_live_state_at_each_write() {
        let live = PersistTag::loaded(accepted(1));
        let holder = live.clone();
        live.settle(&live.capture(), accepted(2), None);
        assert_eq!(
            holder.capture().base,
            accepted(2),
            "no stamp was bound at the clone"
        );
    }
}
