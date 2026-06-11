#![allow(dead_code)]
//! Transaction journal for crash-safe rewind operations (DF-109, AC3).
//!
//! A `RewindTxn` is written atomically to `{session_dir}/rewind_txn.json`
//! **before** each phase of a rewind operation.  On next startup,
//! `reconcile_pending_txns` scans all sessions for incomplete journals and
//! either completes or aborts them, logging at INFO level.
//!
//! The type lives in the domain so future epics (plan execution, MCP tool
//! writes) can reuse the same primitive without depending on the adapter.

use serde::{Deserialize, Serialize};

/// Phase of an in-progress rewind transaction.
///
/// Phases advance monotonically: `Pending → MessagesTruncated → Committed`.
/// `FilesReverted` is reserved for a future mode in which file revert runs
/// before message truncation; the reconciliation logic handles all variants.
///
/// A journal file that exists on disk in state `Committed` is safe to delete.
/// All other states require the described recovery action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindTxnPhase {
    /// Transaction created; no operations started yet.
    /// Recovery: no action needed — delete the journal.
    Pending,
    /// File snapshots reverted; messages not yet truncated.
    /// Recovery: run `truncate_conversation(target_message_index)`.
    FilesReverted,
    /// Messages truncated; file snapshots not yet reverted.
    /// Recovery: run `revert_file_snapshots(target_message_index)`.
    MessagesTruncated,
    /// Both operations completed — transaction committed successfully.
    /// The journal file may be deleted.
    Committed,
}

/// Transaction journal for a single rewind operation.
///
/// Persisted at `{session_dir}/rewind_txn.json`.  One journal per
/// conversation at a time (a second rewind cannot start while one is in
/// progress).  The journal is deleted (or advanced to `Committed`) once
/// the rewind completes successfully.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RewindTxn {
    /// Conversation being rewound.
    pub conversation_id: String,
    /// Target message index for both `truncate_conversation` and
    /// `revert_file_snapshots`.  File reversion uses this directly
    /// (rather than a checkpoint ID) to find all snapshots associated
    /// with messages being removed.
    pub target_message_index: usize,
    /// Current phase of the transaction.
    pub phase: RewindTxnPhase,
    /// Unix seconds when the transaction was created.
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewind_txn_serde_roundtrip() {
        let txn = RewindTxn {
            conversation_id: "conv-abc".to_string(),
            target_message_index: 3,
            phase: RewindTxnPhase::MessagesTruncated,
            created_at: 1700000000,
        };
        let json = serde_json::to_string(&txn).unwrap();
        let decoded: RewindTxn = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.conversation_id, txn.conversation_id);
        assert_eq!(decoded.target_message_index, txn.target_message_index);
        assert_eq!(decoded.phase, txn.phase);
    }

    #[test]
    fn test_rewind_txn_phase_serde_snake_case() {
        let phase = RewindTxnPhase::MessagesTruncated;
        let json = serde_json::to_string(&phase).unwrap();
        assert_eq!(json, r#""messages_truncated""#);
    }

    #[test]
    fn test_rewind_txn_pending_is_default_safe() {
        let txn = RewindTxn {
            conversation_id: "test".to_string(),
            target_message_index: 0,
            phase: RewindTxnPhase::Pending,
            created_at: 0,
        };
        assert_eq!(txn.phase, RewindTxnPhase::Pending);
    }
}
