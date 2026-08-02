//! Versioned records for the single ordered orchestration-room journal.

use serde::{Deserialize, Serialize};

use crate::domain::models::agent_node::NodeCheckpoint;
use crate::domain::models::orchestration_room::RoomEvent;

pub const NODE_JOURNAL_SCHEMA_VERSION: u32 = 1;

fn default_park_concurrency() -> usize {
    crate::domain::models::orchestration::FORK_JOIN_SPAWN_CAP
}

/// One durable line in a room journal. Sequence numbers establish the total
/// order consumed by both recovery and room-projection folds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub schema_version: u32,
    pub seq: u64,
    /// Wall-clock unix milliseconds at which this line was appended
    /// (Story 18.2, AC2).
    ///
    /// **`i64`, not `u64`** — it mirrors [`crate::domain::clock::Clock::wall_now_ms`]
    /// exactly. A `u64` field would force an `as u64` cast at the stamp site
    /// that silently maps negative clock skew to a gigantic positive
    /// timestamp.
    ///
    /// **One stamp site.** The value is supplied by the injected clock inside
    /// the journal's append critical section; emitters never pass it in.
    /// `#[serde(default)]` (0) marks a line journaled before 18.2 — renderers
    /// MUST show that as an explicit unknown, never as 1970.
    #[serde(default)]
    pub recorded_at_ms: i64,
    pub record: JournalRecord,
}

impl JournalEntry {
    /// Construct a line stamped with `recorded_at_ms`.
    ///
    /// Callers outside the journal's append path have no business minting
    /// entries: `NodeJournal::append_records_locked` is the sole production
    /// caller, and it reads the timestamp from the journal's injected clock so
    /// two emitters can never disagree about when a line was written.
    pub fn new(seq: u64, record: JournalRecord, recorded_at_ms: i64) -> Self {
        Self {
            schema_version: NODE_JOURNAL_SCHEMA_VERSION,
            seq,
            recorded_at_ms,
            record,
        }
    }

    /// `true` when this line predates Story 18.2 and carries no timestamp.
    /// Renderers use this to print an explicit unknown rather than epoch zero.
    #[must_use]
    pub fn has_timestamp(&self) -> bool {
        self.recorded_at_ms != 0
    }
}

/// Unforgeable outside this crate: proof that a terminal checkpoint was read
/// back from the fsynced journal prefix.
#[derive(Clone, Debug, PartialEq)]
pub struct JournaledTerminalCheckpoint {
    checkpoint: NodeCheckpoint,
    seq: u64,
}

impl JournaledTerminalCheckpoint {
    pub(crate) fn new(checkpoint: NodeCheckpoint, seq: u64) -> Self {
        debug_assert!(checkpoint.state.is_terminal());
        Self { checkpoint, seq }
    }

    pub fn checkpoint(&self) -> &NodeCheckpoint {
        &self.checkpoint
    }

    pub const fn seq(&self) -> u64 {
        self.seq
    }
}

/// Story 17.2c (D4): a durable snapshot of one capability token's conservation
/// head — the budget/grant state that must survive a restart so spent budget
/// cannot silently reappear and a grant cannot be double-counted. Journaled
/// write-ahead (before the causing mutation's side-effect is observable) and
/// replayed idempotently on recovery (latest snapshot per token wins). The
/// ledger stays synchronous (ADR-14-2-01); durability lives in the caller's
/// write-ahead flush + this record, not in the lock.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerConservationRecord {
    pub token: crate::domain::models::CapabilityTokenId,
    pub total: crate::domain::models::Budget,
    pub available: crate::domain::models::Budget,
    pub consumed: crate::domain::models::Budget,
    pub uses_remaining: Option<u32>,
    pub settled: bool,
    pub revoked: bool,
    /// Story 17.3d-RC-C (AC2): the ledger's nondecreasing authority-time
    /// watermark at journal time, made durable on the SAME conservation stream
    /// so a clock rolled back after a restart cannot revive expired authority.
    #[serde(default)]
    pub authority_time_ms: u64,
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "payload")]
pub enum JournalRecord {
    Checkpoint(NodeCheckpoint),
    Room(RoomEvent),
    /// A stable user-facing alias was durably bound to a node.
    AliasBound {
        node: crate::domain::models::AgentId,
        alias: String,
    },
    /// A terminal node's successor was spawned (a NEW node/alias — never a
    /// revival of the terminal predecessor).
    Successor {
        predecessor: crate::domain::models::AgentId,
        successor: crate::domain::models::AgentId,
        alias: String,
    },
    /// An `Owned` recipient's `MustReport` obligation was accepted at delivery
    /// (durable so a crash before terminal can rebuild the pending set).
    ObligationAccepted {
        node: crate::domain::models::AgentId,
        correlation_id: crate::domain::models::CorrelationId,
    },
    /// A previously accepted `MustReport` obligation was discharged by an
    /// `OwnerReport` (clears the pending set on recovery).
    ObligationDischarged {
        node: crate::domain::models::AgentId,
        correlation_id: crate::domain::models::CorrelationId,
    },
    /// An `Owned` recipient failed to discharge a `MustReport` obligation.
    ObligationViolation {
        node: crate::domain::models::AgentId,
        correlation_id: crate::domain::models::CorrelationId,
    },
    /// A `Waiting` node crossed the persisted-wall-clock dwell threshold. Keyed
    /// by `waiting_since` so re-evaluation after a restart is idempotent (one
    /// hazard per node per waiting epoch).
    HazardRaised {
        node: crate::domain::models::AgentId,
        waiting_since: i64,
        dwell_ms: i64,
    },
    /// Story 17.2c (D4): a durable ledger conservation-head snapshot for one
    /// token, journaled write-ahead on each budget/grant mutation so the head
    /// survives a restart. Replayed idempotently on recovery.
    LedgerConservation(LedgerConservationRecord),
    /// Story 17.2d-b (AC-b1): a fork-join spoke parked on upstream artifacts.
    /// Carries the durable relaunch plan — the full `SpokeSpec` (prompt/tier/
    /// tools/role/waits_for) a checkpoint cannot hold — plus the readiness
    /// edges. `node` is the FULL nonce-qualified tree-node id
    /// (`nonce_qualified(wave_nonce, spoke.id, rerun)`), so the wave nonce is
    /// embedded losslessly in the identity itself (no separate field). Folded
    /// as `Parked − Unparked` on recovery (the `ObligationAccepted/Discharged`
    /// precedent) and consumed by `resume_fork_join_run` at the composition
    /// root — a park record with no consumer is a wiring hole.
    Parked {
        node: crate::domain::models::AgentId,
        producers: Vec<crate::domain::models::AgentId>,
        spec: crate::domain::models::orchestration::SpokeSpec,
        /// Effective concurrency of the original fork-join request. Older
        /// records predate this field and safely fall back to the static cap.
        #[serde(default = "default_park_concurrency")]
        concurrency: usize,
    },
    /// Story 17.2d-b (AC-b1): a previously parked spoke was revived (its
    /// launch adopted the park-time node) or its wave ended without reaching
    /// it. Removes the node from the recovered parked set; replay is
    /// idempotent (an `Unparked` for a never-parked or already-cleared node is
    /// a no-op in the fold).
    Unparked {
        node: crate::domain::models::AgentId,
    },
    /// A short-lived cross-process claim acquired atomically before a parked
    /// spoke is selected for resume. A different claimant may take over only
    /// after `expires_at_ms`; successful adoption clears both park and claim
    /// through `Unparked`.
    ParkClaimed {
        node: crate::domain::models::AgentId,
        claim_id: crate::domain::models::AgentId,
        expires_at_ms: i64,
    },
    /// Releases a failed resume attempt without clearing the durable park.
    /// The fold removes the claim only when `claim_id` is still its owner, so
    /// a late release cannot erase a newer claimant's lease.
    ParkClaimReleased {
        node: crate::domain::models::AgentId,
        claim_id: crate::domain::models::AgentId,
    },
    /// An atomic multi-record group written as ONE journal line. Because a torn
    /// write of a single JSONL line is discarded by the torn-tail repair, a
    /// crash can never persist a PARTIAL group — the whole cascade of terminal
    /// checkpoints is all-or-nothing (R7: no half-killed subtree resurrects a
    /// phantom). Flattened back to individual records at `NodeJournal::load`.
    Batch(Vec<JournalRecord>),
}
