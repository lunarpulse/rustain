//! Versioned records for the single ordered orchestration-room journal.

use serde::{Deserialize, Serialize};

use crate::domain::models::agent_node::NodeCheckpoint;
use crate::domain::models::orchestration_room::RoomEvent;

pub const NODE_JOURNAL_SCHEMA_VERSION: u32 = 1;

/// One durable line in a room journal. Sequence numbers establish the total
/// order consumed by both recovery and room-projection folds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub schema_version: u32,
    pub seq: u64,
    pub record: JournalRecord,
}

impl JournalEntry {
    pub fn new(seq: u64, record: JournalRecord) -> Self {
        Self {
            schema_version: NODE_JOURNAL_SCHEMA_VERSION,
            seq,
            record,
        }
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
    /// An atomic multi-record group written as ONE journal line. Because a torn
    /// write of a single JSONL line is discarded by the torn-tail repair, a
    /// crash can never persist a PARTIAL group — the whole cascade of terminal
    /// checkpoints is all-or-nothing (R7: no half-killed subtree resurrects a
    /// phantom). Flattened back to individual records at `NodeJournal::load`.
    Batch(Vec<JournalRecord>),
}
