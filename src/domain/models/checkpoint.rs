use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Opaque identifier for a checkpoint in a conversation.
///
/// In Story 4-3a, `CheckpointId` maps 1:1 with `message_index` via
/// `CheckpointId(message_index as u64)`.  Full checkpoint creation
/// protocol (create_checkpoint on each turn) is implemented in Story 4-3b.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub u64);

/// Metadata about a checkpoint stored with a conversation.
///
/// Persisted in `{session_dir}/checkpoints.json` as part of the `CheckpointLog`.
/// One checkpoint is created per tool-executing turn, before any tools run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointMeta {
    /// The checkpoint identifier (monotonically increasing, starts at 1).
    pub id: CheckpointId,
    /// Index of the last message included by this checkpoint.
    /// This is the index of the *user* message that triggered the tool-executing turn,
    /// i.e., `messages.len() - 1` at the time the checkpoint was created.
    pub message_index: usize,
    /// Unix seconds when the checkpoint was created.
    pub created_at: i64,
}

/// Result of reverting a single file from a snapshot.
///
/// Not persisted (runtime result type only).
#[derive(Clone, Debug)]
#[allow(dead_code)] // `path` is public API used in E2E tests and future callers
pub struct RevertedFile {
    pub path: PathBuf,
    pub status: RevertStatus,
}

/// Status of a file revert operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevertStatus {
    /// File was successfully restored to its pre-checkpoint content.
    Restored,
    /// File has been externally modified since the snapshot was taken.
    /// The file is left untouched; the user's external edits are preserved.
    Conflict {
        expected_hash: String,
        actual_hash: String,
    },
    /// No snapshot was recorded for this file (snapshot was never taken).
    /// Distinct from `Conflict`: the file simply has no stored history.
    NoSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_id_ord() {
        let a = CheckpointId(0);
        let b = CheckpointId(1);
        let c = CheckpointId(2);
        assert!(a < b);
        assert!(b < c);
        assert!(a < c);
    }

    #[test]
    fn test_checkpoint_id_eq() {
        let a = CheckpointId(42);
        let b = CheckpointId(42);
        let c = CheckpointId(43);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_checkpoint_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(CheckpointId(1));
        set.insert(CheckpointId(2));
        set.insert(CheckpointId(1)); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_checkpoint_id_serde_roundtrip() {
        let original = CheckpointId(99);
        let json = serde_json::to_string(&original).unwrap();
        let decoded: CheckpointId = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_checkpoint_meta_serde_roundtrip() {
        let meta = CheckpointMeta {
            id: CheckpointId(3),
            message_index: 5,
            created_at: 1700000000,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let decoded: CheckpointMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, meta.id);
        assert_eq!(decoded.message_index, meta.message_index);
        assert_eq!(decoded.created_at, meta.created_at);
    }

    #[test]
    fn test_revert_status_conflict_field_equality() {
        let a = RevertStatus::Conflict {
            expected_hash: "abc".to_string(),
            actual_hash: "def".to_string(),
        };
        let b = RevertStatus::Conflict {
            expected_hash: "abc".to_string(),
            actual_hash: "def".to_string(),
        };
        let c = RevertStatus::Conflict {
            expected_hash: "abc".to_string(),
            actual_hash: "xyz".to_string(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_reverted_file_ordering_by_cp_id() {
        // Tests that Vec<RevertedFile> can be created with explicit cp_id ordering
        // (ordering is enforced by the adapter — see Task 6.5; not by this type itself).
        let files = [
            RevertedFile {
                path: PathBuf::from("/a/b.rs"),
                status: RevertStatus::Restored,
            },
            RevertedFile {
                path: PathBuf::from("/a/c.rs"),
                status: RevertStatus::Conflict {
                    expected_hash: "aaa".to_string(),
                    actual_hash: "bbb".to_string(),
                },
            },
        ];
        assert_eq!(files.len(), 2);
        assert!(matches!(files[0].status, RevertStatus::Restored));
        assert!(matches!(files[1].status, RevertStatus::Conflict { .. }));
    }
}
